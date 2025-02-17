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
use grin_keychain::{ChildNumber, Identifier, Keychain};
use crate::wallet::error::ErrorKind;
use crate::wallet::types::{AcctPathMapping, NodeClient, WalletBackend};

/// Get next available key in the wallet for a given parent
pub fn next_available_key<T: ?Sized, C, K>(wallet: &mut T) -> Result<Identifier, Error>
where
    T: WalletBackend<C, K>,
    C: NodeClient,
    K: Keychain,
{
    let child = wallet.next_child()?;
    Ok(child)
}

/// Retrieve an existing key from a wallet
pub fn retrieve_existing_key<T: ?Sized, C, K>(
	wallet: &T,
	key_id: Identifier,
	mmr_index: Option<u64>,
) -> Result<(Identifier, u32), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let existing = wallet.get_output(&key_id, &mmr_index)?;
	let key_id = existing.key_id.clone();
	let derivation = existing.n_child;
	Ok((key_id, derivation))
}

/// Returns a list of account to BIP32 path mappings
pub fn accounts<T: ?Sized, C, K>(wallet: &mut T) -> Result<Vec<AcctPathMapping>, Error>
where
    T: WalletBackend<C, K>,
    C: NodeClient,
    K: Keychain,
{
    Ok(wallet.accounts()?.collect())
}

/// Adds an new parent account path with a given label
pub fn new_acct_path<T: ?Sized, C, K>(wallet: &mut T, label: &str) -> Result<Identifier, Error>
where
    T: WalletBackend<C, K>,
    C: NodeClient,
    K: Keychain,
{
    let label = label.to_string();
    if let Some(_) = wallet.accounts()?.find(|l| l.label == label) {
        return Err(ErrorKind::AccountLabelAlreadyExists(label.clone()).into());
    }

    // We're always using paths at m/k/0 for parent keys for output derivations
    // so find the highest of those, then increment (to conform with external/internal
    // derivation chains in BIP32 spec)

    let highest_entry = wallet.accounts()?.max_by(|a, b| {
        <u32>::from(a.path.to_path().path[0]).cmp(&<u32>::from(b.path.to_path().path[0]))
    });

    let return_id = {
        if let Some(e) = highest_entry {
            let mut p = e.path.to_path();
            p.path[0] = ChildNumber::from(<u32>::from(p.path[0]) + 1);
            p.to_identifier()
        } else {
            K::derive_key_id(2, 0, 0, 0, 0)
        }
    };

    let save_path = AcctPathMapping {
        label: label.to_string(),
        path: return_id.clone(),
    };

    let mut batch = wallet.batch()?;
    batch.save_acct_path(&save_path)?;
    batch.commit()?;
    Ok(return_id)
}

/// Adds/sets a particular account path with a given label
pub fn set_acct_path<T: ?Sized, C, K>(
	wallet: &mut T,
	label: &str,
	path: &Identifier,
) -> Result<(), Error>
where
	T: WalletBackend<C, K>,
	C: NodeClient,
	K: Keychain,
{
	let label = label.to_owned();
	let save_path = AcctPathMapping {
		label: label.to_owned(),
		path: path.clone(),
	};

	let mut batch = wallet.batch()?;
	batch.save_acct_path(&save_path)?;
	batch.commit()?;
	Ok(())
}