// Copyright 2018 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use failure::Error;
use grin_core::core::{Output, TxKernel};
use grin_core::consensus::reward;
use grin_core::global::coinbase_maturity;
use grin_core::libtx::proof::ProofBuilder;
use grin_core::libtx::reward;
use grin_keychain::{Identifier, Keychain, SwitchCommitmentType};
use grin_util::secp::pedersen::Commitment;
use grin_util::from_hex;
use std::collections::HashMap;
use uuid::Uuid;
use crate::wallet::types::{
    BlockFees, CbData, NodeClient, OutputCommitMapping, OutputData, OutputStatus, TxLogEntry,
    TxLogEntryType, WalletBackend, WalletInfo,
};
use super::keys;

/// Retrieve all of the outputs (doesn't attempt to update from node)
pub fn retrieve_outputs<T: ?Sized, C, K>(
	wallet: &mut T,
	include_spent: bool,
	tx_id: Option<u32>,
	parent_key_id: Option<&Identifier>,
) -> Result<Vec<OutputCommitMapping>, Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	// just read the wallet here, no need for a write lock
	let mut outputs = wallet
		.outputs()?
		.filter(|out| include_spent || out.status != OutputStatus::Spent)
		.collect::<Vec<_>>();

	// only include outputs with a given tx_id if provided
	if let Some(id) = tx_id {
		outputs = outputs
			.into_iter()
			.filter(|out| out.tx_log_entry == Some(id))
			.collect::<Vec<_>>();
	}

	if let Some(k) = parent_key_id {
		outputs = outputs
			.iter()
			.filter(|o| o.root_key_id == *k)
			.map(|o| o.clone())
			.collect();
	}

	outputs.sort_by_key(|out| out.n_child);
	let keychain = wallet.keychain().clone();

	let res = outputs
		.into_iter()
		.map(|output| {
			let commit = match output.commit.clone() {
				Some(c) => Commitment::from_vec(from_hex(c).unwrap()),
				None => keychain
					.commit(output.value, &output.key_id, &SwitchCommitmentType::Regular)
					.unwrap(), // TODO: proper support for different switch commitment schemes
			};
			OutputCommitMapping { output, commit }
		})
		.collect();
	Ok(res)
}

/// Retrieve all of the transaction entries, or a particular entry
/// if `parent_key_id` is set, only return entries from that key
pub fn retrieve_txs<T: ?Sized, C, K>(
	wallet: &mut T,
	tx_id: Option<u32>,
	tx_slate_id: Option<Uuid>,
	parent_key_id: Option<&Identifier>,
	outstanding_only: bool,
	check_proofs: bool,
) -> Result<(Vec<TxLogEntry>, HashMap<Uuid, bool>), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let mut txs: Vec<TxLogEntry> = wallet
		.tx_logs()?
		.filter(|tx_entry| {
			let f_pk = match parent_key_id {
				Some(k) => tx_entry.parent_key_id == *k,
				None => true,
			};
			let f_tx_id = match tx_id {
				Some(i) => tx_entry.id == i,
				None => true,
			};
			let f_txs = match tx_slate_id {
				Some(t) => tx_entry.tx_slate_id == Some(t),
				None => true,
			};
			let f_outstanding = match outstanding_only {
				true => {
					!tx_entry.confirmed
						&& (tx_entry.tx_type == TxLogEntryType::TxReceived
							|| tx_entry.tx_type == TxLogEntryType::TxSent)
				}
				false => true,
			};
			f_pk && f_tx_id && f_txs && f_outstanding
		})
		.collect();
	txs.sort_by_key(|tx| tx.creation_ts);

	let mut proofs = HashMap::new();
	if check_proofs {
		for tx in &txs {
			if let Some(slate_id) = &tx.tx_slate_id {
				if wallet.has_stored_tx_proof(&slate_id.to_string()).unwrap_or(false) {
					proofs.insert(slate_id.clone(), true);
				}
			}
		}
	}

	Ok((txs, proofs))
}

/// Refreshes the outputs in a wallet with the latest information
/// from a node
pub fn refresh_outputs<T: ?Sized, C, K>(
	wallet: &mut T,
	parent_key_id: &Identifier,
	update_all: bool,
) -> Result<u64, Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let height = wallet.w2n_client().get_chain_height()?;
	refresh_output_state(wallet, height, parent_key_id, update_all)?;
	Ok(height)
}

/// build a local map of wallet outputs keyed by commit
/// and a list of outputs we want to query the node for
pub fn map_wallet_outputs<T: ?Sized, C, K>(
	wallet: &mut T,
	parent_key_id: &Identifier,
	update_all: bool,
) -> Result<HashMap<Commitment, (Identifier, Option<u64>)>, Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let mut wallet_outputs: HashMap<Commitment, (Identifier, Option<u64>)> =
		HashMap::new();
	let keychain = wallet.keychain().clone();
	let unspents: Vec<OutputData> = wallet
		.outputs()?
		.filter(|x| x.root_key_id == *parent_key_id && x.status != OutputStatus::Spent)
		.collect();

	let (tx_entries, _) = retrieve_txs(wallet, None, None, Some(&parent_key_id), true, false)?;

	// Only select outputs that are actually involved in an outstanding transaction
	let unspents: Vec<OutputData> = match update_all {
		false => unspents
			.into_iter()
			.filter(|x| match x.tx_log_entry.as_ref() {
				Some(t) => {
					if let Some(_) = tx_entries.iter().find(|&te| te.id == *t) {
						true
					} else {
						false
					}
				}
				None => true,
			})
			.collect(),
		true => unspents,
	};

	for out in unspents {
		let commit = match out.commit.clone() {
			Some(c) => Commitment::from_vec(from_hex(c).unwrap()),
			None => keychain
				.commit(out.value, &out.key_id, &SwitchCommitmentType::Regular)
				.unwrap(), // TODO: proper support for different switch commitment schemes
		};
		wallet_outputs.insert(commit, (out.key_id.clone(), out.mmr_index));
	}
	Ok(wallet_outputs)
}

/// Cancel transaction and associated outputs
pub fn cancel_tx_and_outputs<T: ?Sized, C, K>(
	wallet: &mut T,
	tx: TxLogEntry,
	outputs: Vec<OutputData>,
	_parent_key_id: &Identifier,
) -> Result<(), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let mut batch = wallet.batch()?;

	for mut o in outputs {
		// unlock locked outputs
		if o.status == OutputStatus::Unconfirmed {
			batch.delete_output(&o.key_id, &o.mmr_index)?;
		}
		if o.status == OutputStatus::Locked {
			o.status = OutputStatus::Unspent;
			batch.save_output(&o)?;
		}
	}
	let mut tx = tx.clone();
	if tx.tx_type == TxLogEntryType::TxSent {
		tx.tx_type = TxLogEntryType::TxSentCancelled;
	}
	if tx.tx_type == TxLogEntryType::TxReceived {
		tx.tx_type = TxLogEntryType::TxReceivedCancelled;
	}
	batch.save_tx_log_entry(&tx)?;
	batch.commit()?;
	Ok(())
}

/// Apply refreshed API output data to the wallet
pub fn apply_api_outputs<T: ?Sized, C, K>(
	wallet: &mut T,
	wallet_outputs: &HashMap<Commitment, (Identifier, Option<u64>)>,
	api_outputs: &HashMap<Commitment, (String, u64, u64)>,
	height: u64,
	parent_key_id: &Identifier,
) -> Result<(), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	// now for each commit, find the output in the wallet and the corresponding
	// api output (if it exists) and refresh it in-place in the wallet.
	// Note: minimizing the time we spend holding the wallet lock.
	{
		let last_confirmed_height = wallet.get_last_confirmed_height()?;
		// If the server height is less than our confirmed height, don't apply
		// these changes as the chain is syncing, incorrect or forking
		if height < last_confirmed_height {
			warn!(
				"Not updating outputs as the height of the node's chain \
				 is less than the last reported wallet update height."
			);
			warn!("Please wait for sync on node to complete or fork to resolve and try again.");
			return Ok(());
		}
		let mut batch = wallet.batch()?;
		for (commit, (id, mmr_index)) in wallet_outputs.iter() {
			if let Ok(mut output) = wallet.get_output(id, mmr_index) {
				match api_outputs.get(&commit) {
					Some(o) => {
						// if this is a coinbase tx being confirmed, it's recordable in tx log
						if output.is_coinbase && output.status == OutputStatus::Unconfirmed {
							let log_id = batch.next_tx_log_id(parent_key_id)?;
							let mut t = TxLogEntry::new(
								parent_key_id.clone(),
								TxLogEntryType::ConfirmedCoinbase,
								log_id,
							);
							t.confirmed = true;
							t.amount_credited = output.value;
							t.amount_debited = 0;
							t.num_outputs = 1;
							t.update_confirmation_ts();
							output.tx_log_entry = Some(log_id);
							batch.save_tx_log_entry(&t)?;
						}
						// also mark the transaction in which this output is involved as confirmed
						// note that one involved input/output confirmation SHOULD be enough
						// to reliably confirm the tx
						if !output.is_coinbase && output.status == OutputStatus::Unconfirmed {
							let tx = wallet.tx_logs()?.find(|t| {
								Some(t.id) == output.tx_log_entry
									&& t.parent_key_id == *parent_key_id
							});
							if let Some(mut t) = tx {
								t.update_confirmation_ts();
								t.confirmed = true;
								batch.save_tx_log_entry(&t)?;
							}
						}
						output.height = o.1;
						output.mark_unspent();
					}
					None => output.mark_spent(),
				};
				batch.save_output(&output)?;
			}
		}
		batch.save_last_confirmed_height(height)?;
		batch.commit()?;
	}
	Ok(())
}

/// Builds a single api query to retrieve the latest output data from the node.
/// So we can refresh the local wallet outputs.
fn refresh_output_state<T: ?Sized, C, K>(
	wallet: &mut T,
	height: u64,
	parent_key_id: &Identifier,
	update_all: bool,
) -> Result<(), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	debug!("Refreshing wallet outputs");

	// build a local map of wallet outputs keyed by commit
	// and a list of outputs we want to query the node for
	let wallet_outputs = map_wallet_outputs(wallet, parent_key_id, update_all)?;

	let wallet_output_keys = wallet_outputs.keys().map(|commit| commit.clone()).collect();

	let api_outputs = wallet
		.w2n_client()
		.get_outputs_from_node(wallet_output_keys)?;
	apply_api_outputs(wallet, &wallet_outputs, &api_outputs, height, parent_key_id)?;
	clean_old_unconfirmed(wallet, height)?;
	Ok(())
}

fn clean_old_unconfirmed<T: ?Sized, C, K>(wallet: &mut T, height: u64) -> Result<(), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	if height < 50 {
		return Ok(());
	}
	let mut ids_to_del = vec![];
	for out in wallet.outputs()? {
		if out.status == OutputStatus::Unconfirmed
			&& out.height > 0
			&& out.height < height - 50
			&& out.is_coinbase
		{
			ids_to_del.push(out.key_id.clone())
		}
	}
	let mut batch = wallet.batch()?;
	for id in ids_to_del {
		batch.delete_output(&id, &None)?;
	}
	batch.commit()?;
	Ok(())
}

/// Retrieve summary info about the wallet
/// caller should refresh first if desired
pub fn retrieve_info<T: ?Sized, C, K>(
	wallet: &mut T,
	parent_key_id: &Identifier,
	minimum_confirmations: u64,
) -> Result<WalletInfo, Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let current_height = wallet.get_last_confirmed_height()?;
	let outputs = wallet
		.outputs()?
		.filter(|out| out.root_key_id == *parent_key_id);

	let mut unspent_total = 0;
	let mut immature_total = 0;
	let mut awaiting_finalization_total = 0;
	let mut unconfirmed_total = 0;
	let mut locked_total = 0;

	for out in outputs {
		match out.status {
			OutputStatus::Unspent => {
				if out.is_coinbase && out.lock_height > current_height {
					immature_total += out.value;
				} else if out.num_confirmations(current_height) < minimum_confirmations {
					// Treat anything less than minimum confirmations as "unconfirmed".
					unconfirmed_total += out.value;
				} else {
					unspent_total += out.value;
				}
			}
			OutputStatus::Unconfirmed => {
				// We ignore unconfirmed coinbase outputs completely.
				if !out.is_coinbase {
					if minimum_confirmations == 0 {
						unconfirmed_total += out.value;
					} else {
						awaiting_finalization_total += out.value;
					}
				}
			}
			OutputStatus::Locked => {
				locked_total += out.value;
			}
			OutputStatus::Spent => {}
		}
	}

	Ok(WalletInfo {
		last_confirmed_height: current_height,
		minimum_confirmations,
		total: unspent_total + unconfirmed_total + immature_total,
		amount_awaiting_finalization: awaiting_finalization_total,
		amount_awaiting_confirmation: unconfirmed_total,
		amount_immature: immature_total,
		amount_locked: locked_total,
		amount_currently_spendable: unspent_total,
	})
}

/// Build a coinbase output and insert into wallet
pub fn build_coinbase<T: ?Sized, C, K>(
	wallet: &mut T,
	block_fees: &BlockFees,
) -> Result<CbData, Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let (out, kern, block_fees) = receive_coinbase(wallet, block_fees)?;

	Ok(CbData {
		output: out,
		kernel: kern,
		key_id: block_fees.key_id,
	})
}

//TODO: Split up the output creation and the wallet insertion
/// Build a coinbase output and the corresponding kernel
pub fn receive_coinbase<T: ?Sized, C, K>(
	wallet: &mut T,
	block_fees: &BlockFees,
) -> Result<(Output, TxKernel, BlockFees), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let height = block_fees.height;
	let lock_height = height + coinbase_maturity();
	let key_id = block_fees.key_id();
	let parent_key_id = wallet.get_parent_key_id();

	let key_id = match key_id {
		Some(key_id) => match keys::retrieve_existing_key(wallet, key_id, None) {
			Ok(k) => k.0,
			Err(_) => keys::next_available_key(wallet)?,
		},
		None => keys::next_available_key(wallet)?,
	};

	{
		// Now acquire the wallet lock and write the new output.
		let amount = reward(block_fees.fees);
		let commit = wallet.calc_commit_for_cache(amount, &key_id)?;
		let mut batch = wallet.batch()?;
		batch.save_output(&OutputData {
			root_key_id: parent_key_id,
			key_id: key_id.clone(),
			n_child: key_id.to_path().last_path_index(),
			mmr_index: None,
			commit,
			value: amount,
			status: OutputStatus::Unconfirmed,
			height,
			lock_height,
			is_coinbase: true,
			tx_log_entry: None,
		})?;
		batch.commit()?;
	}

	debug!(
		"receive_coinbase: built candidate output - {:?}, {}",
		key_id.clone(),
		key_id,
	);

	let mut block_fees = block_fees.clone();
	block_fees.key_id = Some(key_id.clone());

	debug!("receive_coinbase: {:?}", block_fees);

	let keychain = wallet.keychain();
	let (out, kern) = reward::output(
		keychain,
		&ProofBuilder::new(keychain),
		&key_id,
		block_fees.fees,
		false
	)?;
	Ok((out, kern, block_fees))
}


/// Fetch the height of the node
/// If the node is down, use the height of the latest output
pub fn node_height<T: ?Sized, C, K>(
	wallet: &mut T,
) -> Result<(bool, u64), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let res = wallet.w2n_client().get_chain_height();
	match res {
		Ok(height) => Ok((true, height)),
		Err(_) => {
			let outputs = retrieve_outputs(wallet, true, None, None)?;
			let height = match outputs.iter().map(|m| m.output.height).max() {
				Some(height) => height,
				None => 0,
			};
			Ok((false, height))
		}
	}
}