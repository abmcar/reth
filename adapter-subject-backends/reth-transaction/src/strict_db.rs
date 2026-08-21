//! Proof-shaped in-memory database that never synthesizes missing state.

use alloy_primitives::{Address, B256, U256};
use revm::{
    bytecode::Bytecode,
    context::DBErrorMarker,
    state::{AccountId, AccountInfo},
    Database,
};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// Ordered database calls made by the transaction shell and journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DbAccess {
    Basic(Address),
    Code(B256),
    Storage(Address, U256),
    StorageByAccountId(Address, AccountId, U256),
    BlockHash(u64),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum StrictDbError {
    #[error("account is outside the proven witness: {0}")]
    MissingAccount(Address),
    #[error("code is outside the proven witness: {0}")]
    MissingCode(B256),
    #[error("code witness hash mismatch: expected {expected}, got {actual}")]
    CodeHashMismatch { expected: B256, actual: B256 },
    #[error("storage is outside the proven witness: {0}[{1}]")]
    MissingStorage(Address, U256),
    #[error("an absent account cannot have nonzero proven storage: {0}[{1}]={2}")]
    NonzeroStorageForAbsentAccount(Address, U256, U256),
    #[error("block hash is outside the proven witness: {0}")]
    MissingBlockHash(u64),
    #[error("account id {account_id:?} is not proven for {address}")]
    InvalidAccountId {
        address: Address,
        account_id: AccountId,
    },
    #[error("account id {account_id:?} is already assigned to another address")]
    DuplicateAccountId { account_id: AccountId },
}

impl DBErrorMarker for StrictDbError {}

/// Explicit coverage and values for one transaction witness.
#[derive(Clone, Debug, Default)]
pub struct StrictDb {
    covered_accounts: BTreeSet<Address>,
    accounts: BTreeMap<Address, AccountInfo>,
    codes: BTreeMap<B256, Bytecode>,
    covered_storage: BTreeSet<(Address, U256)>,
    storage: BTreeMap<(Address, U256), U256>,
    account_ids: BTreeMap<AccountId, Address>,
    loaded_account_ids: BTreeSet<(Address, AccountId)>,
    block_hashes: BTreeMap<u64, B256>,
    accesses: Vec<DbAccess>,
}

impl StrictDb {
    pub fn cover_absent_account(&mut self, address: Address) {
        self.covered_accounts.insert(address);
        self.accounts.remove(&address);
        self.covered_storage
            .retain(|(covered, _)| *covered != address);
        self.storage.retain(|(covered, _), _| *covered != address);
        self.account_ids.retain(|_, covered| *covered != address);
        self.loaded_account_ids
            .retain(|(covered, _)| *covered != address);
    }

    pub fn insert_account(
        &mut self,
        address: Address,
        info: AccountInfo,
    ) -> Result<(), StrictDbError> {
        if let Some(code) = info.code.as_ref() {
            let actual = code.hash_slow();
            if actual != info.code_hash {
                return Err(StrictDbError::CodeHashMismatch {
                    expected: info.code_hash,
                    actual,
                });
            }
        }
        if let Some(account_id) = info.account_id {
            if self
                .account_ids
                .get(&account_id)
                .is_some_and(|covered| *covered != address)
            {
                return Err(StrictDbError::DuplicateAccountId { account_id });
            }
        }
        self.account_ids.retain(|_, covered| *covered != address);
        self.loaded_account_ids
            .retain(|(covered, _)| *covered != address);
        self.covered_storage
            .retain(|(covered, _)| *covered != address);
        self.storage.retain(|(covered, _), _| *covered != address);
        if let Some(account_id) = info.account_id {
            self.account_ids.insert(account_id, address);
        }
        if let Some(code) = info.code.as_ref() {
            self.codes.insert(info.code_hash, code.clone());
        }
        self.covered_accounts.insert(address);
        self.accounts.insert(address, info);
        Ok(())
    }

    /// Covers a slot even when its canonical value is zero.
    pub fn cover_storage(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<(), StrictDbError> {
        if !self.covered_accounts.contains(&address) {
            return Err(StrictDbError::MissingAccount(address));
        }
        if !self.accounts.contains_key(&address) && !value.is_zero() {
            return Err(StrictDbError::NonzeroStorageForAbsentAccount(
                address, key, value,
            ));
        }
        self.covered_storage.insert((address, key));
        self.storage.insert((address, key), value);
        Ok(())
    }

    pub fn insert_code(&mut self, hash: B256, code: Bytecode) -> Result<(), StrictDbError> {
        let actual = code.hash_slow();
        if actual != hash {
            return Err(StrictDbError::CodeHashMismatch {
                expected: hash,
                actual,
            });
        }
        self.codes.insert(hash, code);
        Ok(())
    }

    pub fn insert_block_hash(&mut self, number: u64, hash: B256) {
        self.block_hashes.insert(number, hash);
    }

    pub fn accesses(&self) -> &[DbAccess] {
        &self.accesses
    }

    pub fn clear_accesses(&mut self) {
        self.accesses.clear();
    }

    fn storage_value(&self, address: Address, key: U256) -> Result<U256, StrictDbError> {
        if !self.covered_storage.contains(&(address, key)) {
            return Err(StrictDbError::MissingStorage(address, key));
        }
        Ok(self
            .storage
            .get(&(address, key))
            .copied()
            .unwrap_or_default())
    }
}

impl Database for StrictDb {
    type Error = StrictDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.accesses.push(DbAccess::Basic(address));
        if !self.covered_accounts.contains(&address) {
            return Err(StrictDbError::MissingAccount(address));
        }
        let info = self.accounts.get(&address).cloned();
        if let Some(account_id) = info.as_ref().and_then(|info| info.account_id) {
            self.loaded_account_ids.insert((address, account_id));
        }
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.accesses.push(DbAccess::Code(code_hash));
        self.codes
            .get(&code_hash)
            .cloned()
            .ok_or(StrictDbError::MissingCode(code_hash))
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.accesses.push(DbAccess::Storage(address, index));
        self.storage_value(address, index)
    }

    fn storage_by_account_id(
        &mut self,
        address: Address,
        account_id: AccountId,
        storage_key: U256,
    ) -> Result<U256, Self::Error> {
        self.accesses.push(DbAccess::StorageByAccountId(
            address,
            account_id,
            storage_key,
        ));
        if self.account_ids.get(&account_id) != Some(&address)
            || !self.loaded_account_ids.contains(&(address, account_id))
        {
            return Err(StrictDbError::InvalidAccountId {
                address,
                account_id,
            });
        }
        self.storage_value(address, storage_key)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.accesses.push(DbAccess::BlockHash(number));
        self.block_hashes
            .get(&number)
            .copied()
            .ok_or(StrictDbError::MissingBlockHash(number))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_storage_requires_explicit_coverage_on_both_entry_points() {
        let address = Address::repeat_byte(0x11);
        let account_id = AccountId::new(7).unwrap();
        let mut db = StrictDb::default();
        db.insert_account(
            address,
            AccountInfo {
                account_id: Some(account_id),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(matches!(
            db.storage(address, U256::ZERO),
            Err(StrictDbError::MissingStorage(..))
        ));
        db.basic(address).unwrap();
        assert!(matches!(
            db.storage_by_account_id(address, account_id, U256::ZERO),
            Err(StrictDbError::MissingStorage(..))
        ));

        db.cover_storage(address, U256::ZERO, U256::ZERO).unwrap();
        assert_eq!(db.storage(address, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(
            db.storage_by_account_id(address, account_id, U256::ZERO)
                .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn account_id_cannot_bypass_address_binding() {
        let address = Address::repeat_byte(0x11);
        let other = Address::repeat_byte(0x22);
        let account_id = AccountId::new(9).unwrap();
        let mut db = StrictDb::default();
        db.insert_account(
            address,
            AccountInfo {
                account_id: Some(account_id),
                ..Default::default()
            },
        )
        .unwrap();
        db.cover_absent_account(other);
        db.cover_storage(address, U256::ZERO, U256::ZERO).unwrap();
        db.basic(address).unwrap();

        assert!(matches!(
            db.storage_by_account_id(other, account_id, U256::ZERO),
            Err(StrictDbError::InvalidAccountId { .. })
        ));
    }

    #[test]
    fn account_id_must_come_from_a_successful_basic_load() {
        let address = Address::repeat_byte(0x11);
        let account_id = AccountId::new(3).unwrap();
        let mut db = StrictDb::default();
        db.insert_account(
            address,
            AccountInfo {
                account_id: Some(account_id),
                ..Default::default()
            },
        )
        .unwrap();
        db.cover_storage(address, U256::ZERO, U256::ZERO).unwrap();

        assert!(matches!(
            db.storage_by_account_id(address, account_id, U256::ZERO),
            Err(StrictDbError::InvalidAccountId { .. })
        ));
        db.basic(address).unwrap();
        assert_eq!(
            db.storage_by_account_id(address, account_id, U256::ZERO)
                .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn absent_accounts_only_accept_explicit_zero_storage_and_code_hashes_are_verified() {
        let address = Address::repeat_byte(0x44);
        let mut db = StrictDb::default();
        db.cover_absent_account(address);
        db.cover_storage(address, U256::ZERO, U256::ZERO)
            .expect("account exclusion proves every storage slot is zero");
        assert_eq!(db.storage(address, U256::ZERO), Ok(U256::ZERO));
        assert_eq!(
            db.cover_storage(address, U256::from(1), U256::from(1)),
            Err(StrictDbError::NonzeroStorageForAbsentAccount(
                address,
                U256::from(1),
                U256::from(1)
            ))
        );

        let code = Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x00]));
        assert!(matches!(
            db.insert_code(B256::ZERO, code),
            Err(StrictDbError::CodeHashMismatch { .. })
        ));
    }

    #[test]
    fn replacing_an_account_clears_old_identity_and_storage_witnesses() {
        let address = Address::repeat_byte(0x55);
        let old_id = AccountId::new(5).unwrap();
        let new_id = AccountId::new(6).unwrap();
        let mut db = StrictDb::default();
        db.insert_account(
            address,
            AccountInfo {
                account_id: Some(old_id),
                ..Default::default()
            },
        )
        .unwrap();
        db.basic(address).unwrap();
        db.cover_storage(address, U256::ZERO, U256::from(99))
            .unwrap();
        db.insert_account(
            address,
            AccountInfo {
                account_id: Some(new_id),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(matches!(
            db.storage_by_account_id(address, old_id, U256::ZERO),
            Err(StrictDbError::InvalidAccountId { .. })
        ));
        assert!(matches!(
            db.storage_by_account_id(address, new_id, U256::ZERO),
            Err(StrictDbError::InvalidAccountId { .. })
        ));
        db.basic(address).unwrap();
        assert!(matches!(
            db.storage_by_account_id(address, new_id, U256::ZERO),
            Err(StrictDbError::MissingStorage(..))
        ));
        db.cover_storage(address, U256::ZERO, U256::ZERO).unwrap();
        assert_eq!(
            db.storage_by_account_id(address, new_id, U256::ZERO)
                .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn every_uncovered_database_namespace_fails_and_records_the_attempt() {
        let address = Address::repeat_byte(0x66);
        let code_hash = B256::repeat_byte(0x77);
        let mut db = StrictDb::default();

        assert_eq!(
            db.basic(address),
            Err(StrictDbError::MissingAccount(address))
        );
        assert_eq!(
            db.code_by_hash(code_hash),
            Err(StrictDbError::MissingCode(code_hash))
        );
        assert_eq!(
            db.storage(address, U256::from(8)),
            Err(StrictDbError::MissingStorage(address, U256::from(8)))
        );
        assert_eq!(db.block_hash(9), Err(StrictDbError::MissingBlockHash(9)));
        assert_eq!(
            db.accesses(),
            [
                DbAccess::Basic(address),
                DbAccess::Code(code_hash),
                DbAccess::Storage(address, U256::from(8)),
                DbAccess::BlockHash(9),
            ]
        );
    }
}
