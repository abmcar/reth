//! Verified, fail-closed database construction from a Reth execution witness.
//!
//! `ExecutionWitness::keys` is auxiliary preimage data, not an exact access
//! list. Instead of treating it as an allowlist, [`WitnessDb`] resolves each
//! database query directly against the verified sparse trie. An optional
//! account-qualified [`AccessManifest`] can eagerly preload known paths.

pub mod batch;
pub mod evmone_batch;
pub mod replay;

use alloy_consensus::{
    proofs, Block as ConsensusBlock, EthereumTxEnvelope, Header, TrieAccount, TxEip4844,
};
use alloy_primitives::{keccak256, map::B256Map, Address, Bytes, B256, U256};
use alloy_rlp::{Decodable, Encodable};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_trie::{
    nodes::{
        BranchNode, BranchNodeRef, ExtensionNodeRef, LeafNodeRef, RlpNode, TrieNode,
        CHILD_INDEX_RANGE,
    },
    proof::verify_proof,
    TrieMask, EMPTY_ROOT_HASH, KECCAK_EMPTY,
};
use reth_dtvm_transaction_adapter::{StrictDb, StrictDbError};
use reth_trie_common::{DecodedMultiProofV2, HashedPostState, KeccakKeyHasher, Nibbles};
use reth_trie_sparse::{
    LeafLookup, LeafLookupError, LeafUpdate, RevealableSparseTrie, SparseStateTrie,
    SparseTrie as SparseTrieTrait,
};
use revm::{
    bytecode::Bytecode, context::DBErrorMarker, database::BundleState, state::AccountInfo, Database,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// An explicitly observed storage access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageAccess {
    pub address: Address,
    /// Unhashed, 32-byte EVM storage key.
    pub slot: B256,
}

/// Account-qualified access targets captured while the reference client
/// executes the block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessManifest {
    #[serde(default)]
    pub accounts: Vec<Address>,
    #[serde(default)]
    pub storage: Vec<StorageAccess>,
}

/// Serializable input that binds a standard execution witness to its target
/// block, with optional access targets for eager preloading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WitnessBundle {
    /// Canonical RLP of the header for the block being replayed.
    pub target_header: Bytes,
    /// Independently selected canonical block hash.
    pub target_block_hash: B256,
    /// Optional canonical RLP of the complete target block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_block: Option<Bytes>,
    pub witness: ExecutionWitness,
    #[serde(default)]
    pub access_manifest: AccessManifest,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum WitnessImportError {
    #[error("invalid witness bundle JSON: {0}")]
    InvalidJson(String),
    #[error("target header is not canonical RLP: {0}")]
    InvalidTargetHeader(String),
    #[error("target header has trailing bytes after canonical RLP")]
    TargetHeaderTrailingBytes,
    #[error("target block hash mismatch: expected {expected}, got {actual}")]
    TargetBlockHashMismatch { expected: B256, actual: B256 },
    #[error("target block is not valid RLP: {0}")]
    InvalidTargetBlock(String),
    #[error("target block has trailing bytes after canonical RLP")]
    TargetBlockTrailingBytes,
    #[error("target block RLP is not canonical")]
    TargetBlockNonCanonical,
    #[error("target block header does not match targetHeader")]
    TargetBlockHeaderMismatch,
    #[error("raw target block hash mismatch: expected {expected}, got {actual}")]
    TargetRawBlockHashMismatch { expected: B256, actual: B256 },
    #[error("target block transactions root mismatch: expected {expected}, got {actual}")]
    TargetBlockTransactionsRootMismatch { expected: B256, actual: B256 },
    #[error("target block ommers hash mismatch: expected {expected}, got {actual}")]
    TargetBlockOmmersHashMismatch { expected: B256, actual: B256 },
    #[error("target block withdrawals root mismatch: expected {expected:?}, got {actual:?}")]
    TargetBlockWithdrawalsRootMismatch {
        expected: Option<B256>,
        actual: Option<B256>,
    },
    #[error("target block zero has no parent execution state")]
    GenesisTarget,
    #[error("execution witness has no ancestor headers")]
    MissingHeaders,
    #[error("header {index} is not canonical RLP: {reason}")]
    InvalidHeader { index: usize, reason: String },
    #[error("header {index} has trailing bytes after canonical RLP")]
    HeaderTrailingBytes { index: usize },
    #[error(
        "header chain is not contiguous at index {index}: expected number {expected}, got {actual}"
    )]
    HeaderNumberDiscontinuity {
        index: usize,
        expected: u64,
        actual: u64,
    },
    #[error(
        "header chain parent hash mismatch at index {index}: expected {expected}, got {actual}"
    )]
    HeaderParentMismatch {
        index: usize,
        expected: B256,
        actual: B256,
    },
    #[error("target parent number mismatch: expected {expected}, got {actual}")]
    TargetParentNumberMismatch { expected: u64, actual: u64 },
    #[error("target parent hash mismatch: expected {expected}, got {actual}")]
    TargetParentHashMismatch { expected: B256, actual: B256 },
    #[error("witness key {index} has invalid preimage length {length}; expected 20 or 32")]
    InvalidKeyLength { index: usize, length: usize },
    #[error("parent state root {0} is not present in the witness node store")]
    MissingStateRoot(B256),
    #[error("failed to reconstruct witness multiproof: {0}")]
    InvalidMultiproof(String),
    #[error("failed to reveal witness multiproof: {0}")]
    InvalidSparseTrie(String),
    #[error("revealed pre-state root mismatch: expected {expected}, got {actual}")]
    PreStateRootMismatch { expected: B256, actual: B256 },
    #[error("account proof is incomplete for {0}")]
    IncompleteAccountProof(Address),
    #[error("account leaf for {address} is invalid RLP: {reason}")]
    InvalidAccountLeaf { address: Address, reason: String },
    #[error("account leaf for {0} has trailing RLP bytes")]
    AccountLeafTrailingBytes(Address),
    #[error("storage manifest names an account that was not resolved: {0}")]
    StorageAccountNotResolved(Address),
    #[error("storage proof is incomplete for {address}[{slot}]")]
    IncompleteStorageProof { address: Address, slot: B256 },
    #[error("storage leaf for {address}[{slot}] is invalid RLP: {reason}")]
    InvalidStorageLeaf {
        address: Address,
        slot: B256,
        reason: String,
    },
    #[error("storage leaf for {address}[{slot}] has trailing RLP bytes")]
    StorageLeafTrailingBytes { address: Address, slot: B256 },
    #[error(
        "post-state account proof is incomplete for {hashed_address} at target {proof_target}"
    )]
    PostStateAccountProofIncomplete {
        hashed_address: B256,
        proof_target: B256,
    },
    #[error(
        "post-state storage proof is incomplete for {hashed_address}[{hashed_slot}] at target {proof_target}"
    )]
    PostStateStorageProofIncomplete {
        hashed_address: B256,
        hashed_slot: B256,
        proof_target: B256,
    },
    #[error("post-state account leaf for {hashed_address} is invalid: {reason}")]
    PostStateInvalidAccountLeaf {
        hashed_address: B256,
        reason: String,
    },
    #[error("post-state storage leaf for {hashed_address}[{hashed_slot}] is invalid: {reason}")]
    PostStateInvalidStorageLeaf {
        hashed_address: B256,
        hashed_slot: B256,
        reason: String,
    },
    #[error("post-state storage update for {0} has no final account")]
    PostStateMissingFinalAccount(B256),
    #[error("post-state sparse trie update failed in {scope}: {reason}")]
    PostStateSparseTrie { scope: &'static str, reason: String },
    #[error("post-state root mismatch: expected {expected}, got {actual}")]
    PostStateRootMismatch { expected: B256, actual: B256 },
    #[error(transparent)]
    StrictDatabase(#[from] StrictDbError),
}

/// A REVM database whose values and explicit zero/absence results were proven
/// against the target block's parent state root.
#[derive(Debug)]
pub struct WitnessDb {
    strict: StrictDb,
    sparse: SparseStateTrie,
    flat_nodes: B256Map<Bytes>,
    target_header: Header,
    target_block: Option<Bytes>,
    target_block_transaction_count: Option<usize>,
    parent_header: Header,
    pre_state_root: B256,
    manifest: AccessManifest,
    resolved_accounts: BTreeSet<Address>,
    resolved_storage: BTreeSet<(Address, U256)>,
}

impl WitnessDb {
    /// Parse and verify a JSON bundle.
    pub fn from_json(json: &[u8]) -> Result<Self, WitnessImportError> {
        let bundle = serde_json::from_slice(json)
            .map_err(|error| WitnessImportError::InvalidJson(error.to_string()))?;
        Self::from_bundle(bundle)
    }

    /// Verify headers and trie nodes, then optionally preload the access
    /// manifest into a fail-closed database.
    pub fn from_bundle(bundle: WitnessBundle) -> Result<Self, WitnessImportError> {
        let target_header = decode_target_header(&bundle.target_header)?;
        let actual_target_hash = target_header.hash_slow();
        if actual_target_hash != bundle.target_block_hash {
            return Err(WitnessImportError::TargetBlockHashMismatch {
                expected: bundle.target_block_hash,
                actual: actual_target_hash,
            });
        }
        if target_header.number == 0 {
            return Err(WitnessImportError::GenesisTarget);
        }
        let target_block_transaction_count = bundle
            .target_block
            .as_ref()
            .map(|raw| validate_target_block(raw, &target_header, bundle.target_block_hash))
            .transpose()?;

        validate_key_preimages(&bundle.witness.keys)?;
        let headers = decode_and_validate_headers(
            &bundle.witness.headers,
            target_header.number,
            target_header.parent_hash,
        )?;
        let parent_header = headers
            .last()
            .cloned()
            .ok_or(WitnessImportError::MissingHeaders)?;
        let pre_state_root = parent_header.state_root;
        let flat_nodes = flat_node_store(&bundle.witness.state);
        let sparse = reveal_sparse_trie(pre_state_root, &flat_nodes)?;
        let mut strict = StrictDb::default();

        for code in &bundle.witness.codes {
            let hash = keccak256(code);
            strict.insert_code(hash, Bytecode::new_raw(code.clone()))?;
        }
        for header in &headers {
            strict.insert_block_hash(header.number, header.hash_slow());
        }

        let manifest = bundle.access_manifest;
        let mut account_targets = BTreeSet::from_iter(manifest.accounts.iter().copied());
        account_targets.extend(manifest.storage.iter().map(|target| target.address));
        let storage_targets = manifest.storage.clone();

        let mut db = Self {
            strict,
            sparse,
            flat_nodes,
            target_header,
            target_block: bundle.target_block,
            target_block_transaction_count,
            parent_header,
            pre_state_root,
            manifest,
            resolved_accounts: BTreeSet::new(),
            resolved_storage: BTreeSet::new(),
        };

        for address in account_targets {
            db.resolve_account(address)?;
        }
        for target in storage_targets {
            db.resolve_storage(target.address, U256::from_be_bytes(target.slot.0))?;
        }

        // Recompute after all reads to prove that lookup did not mutate the
        // sparse trie and that the root remains anchored.
        let actual_root = db
            .sparse
            .root()
            .map_err(|error| WitnessImportError::InvalidSparseTrie(error.to_string()))?;
        if actual_root != pre_state_root {
            return Err(WitnessImportError::PreStateRootMismatch {
                expected: pre_state_root,
                actual: actual_root,
            });
        }

        Ok(db)
    }

    pub const fn pre_state_root(&self) -> B256 {
        self.pre_state_root
    }

    pub const fn target_header(&self) -> &Header {
        &self.target_header
    }

    pub const fn target_block(&self) -> Option<&Bytes> {
        self.target_block.as_ref()
    }

    pub const fn target_block_transaction_count(&self) -> Option<usize> {
        self.target_block_transaction_count
    }

    pub const fn parent_header(&self) -> &Header {
        &self.parent_header
    }

    pub const fn access_manifest(&self) -> &AccessManifest {
        &self.manifest
    }

    pub const fn strict_db(&self) -> &StrictDb {
        &self.strict
    }

    pub fn strict_db_mut(&mut self) -> &mut StrictDb {
        &mut self.strict
    }

    pub fn into_strict_db(self) -> StrictDb {
        self.strict
    }

    fn resolve_account(&mut self, address: Address) -> Result<(), WitnessImportError> {
        if self.resolved_accounts.contains(&address) {
            return Ok(());
        }

        match self.authenticated_account(address)? {
            Some(account) => {
                let code = if account.code_hash == KECCAK_EMPTY {
                    Some(Bytecode::default())
                } else {
                    None
                };
                self.strict.insert_account(
                    address,
                    AccountInfo {
                        balance: account.balance,
                        nonce: account.nonce,
                        code_hash: account.code_hash,
                        account_id: None,
                        code,
                    },
                )?;
            }
            None => self.strict.cover_absent_account(address),
        }
        self.resolved_accounts.insert(address);
        Ok(())
    }

    fn authenticated_account(
        &self,
        address: Address,
    ) -> Result<Option<TrieAccount>, WitnessImportError> {
        let state_trie = self
            .sparse
            .state_trie_ref()
            .ok_or(WitnessImportError::MissingStateRoot(self.pre_state_root))?;
        let path = Nibbles::unpack(keccak256(address));
        let value = match state_trie.find_leaf(&path, None) {
            Ok(LeafLookup::Exists) => Some(
                state_trie
                    .get_leaf_value(&path)
                    .cloned()
                    .ok_or(WitnessImportError::IncompleteAccountProof(address))?,
            ),
            Ok(LeafLookup::NonExistent) => None,
            Err(LeafLookupError::BlindedNode { .. }) => {
                verified_flat_trie_lookup(self.pre_state_root, path, &self.flat_nodes)
                    .map_err(|_| WitnessImportError::IncompleteAccountProof(address))?
            }
            Err(LeafLookupError::ValueMismatch { .. }) => {
                return Err(WitnessImportError::IncompleteAccountProof(address));
            }
        };
        value
            .as_deref()
            .map(|value| decode_account(address, value))
            .transpose()
    }

    fn resolve_storage(&mut self, address: Address, slot: U256) -> Result<(), WitnessImportError> {
        if self.resolved_storage.contains(&(address, slot)) {
            return Ok(());
        }

        self.resolve_account(address)?;
        let hashed_address = keccak256(address);
        let value = match self.authenticated_account(address)? {
            None => {
                // A complete account exclusion proof establishes that all
                // storage under this address is zero.
                U256::ZERO
            }
            Some(account) => {
                if account.storage_root == EMPTY_ROOT_HASH {
                    U256::ZERO
                } else {
                    resolve_storage(
                        &self.sparse,
                        &self.flat_nodes,
                        &StorageAccess {
                            address,
                            slot: B256::from(slot.to_be_bytes::<32>()),
                        },
                        hashed_address,
                        account.storage_root,
                    )?
                }
            }
        };
        self.strict.cover_storage(address, slot, value)?;
        self.resolved_storage.insert((address, slot));
        Ok(())
    }

    /// The verified sparse trie is retained for the later post-state-root
    /// phase. Phase 1 exposes only its anchored root.
    pub fn verified_root(&mut self) -> Result<B256, WitnessImportError> {
        self.sparse
            .root()
            .map_err(|error| WitnessImportError::InvalidSparseTrie(error.to_string()))
    }

    /// Consume this witness database and prove the bundle's final state root against the target
    /// header.
    pub fn into_verified_post_state_root(
        mut self,
        bundle: &BundleState,
    ) -> Result<B256, WitnessImportError> {
        let post_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
        let mut changed_addresses = BTreeSet::new();
        changed_addresses.extend(post_state.accounts.keys().copied());
        changed_addresses.extend(post_state.storages.keys().copied());

        // Cache every authenticated old account leaf before mutating either trie.
        let mut old_accounts = BTreeMap::new();
        {
            let state_trie = self.sparse.state_trie_ref().ok_or(
                WitnessImportError::PostStateAccountProofIncomplete {
                    hashed_address: B256::ZERO,
                    proof_target: B256::ZERO,
                },
            )?;
            for hashed_address in &changed_addresses {
                let path = Nibbles::unpack(*hashed_address);
                let old_account = match state_trie.find_leaf(&path, None) {
                    Ok(LeafLookup::Exists) => {
                        let value = state_trie.get_leaf_value(&path).ok_or(
                            WitnessImportError::PostStateAccountProofIncomplete {
                                hashed_address: *hashed_address,
                                proof_target: *hashed_address,
                            },
                        )?;
                        Some(decode_post_state_account_leaf(*hashed_address, value)?)
                    }
                    Ok(LeafLookup::NonExistent) => None,
                    Err(LeafLookupError::BlindedNode { .. }) => {
                        let value =
                            verified_flat_trie_lookup(self.pre_state_root, path, &self.flat_nodes)
                                .map_err(|_| {
                                    WitnessImportError::PostStateAccountProofIncomplete {
                                        hashed_address: *hashed_address,
                                        proof_target: *hashed_address,
                                    }
                                })?;
                        value
                            .as_deref()
                            .map(|value| decode_post_state_account_leaf(*hashed_address, value))
                            .transpose()?
                    }
                    Err(LeafLookupError::ValueMismatch { .. }) => {
                        return Err(WitnessImportError::PostStateAccountProofIncomplete {
                            hashed_address: *hashed_address,
                            proof_target: *hashed_address,
                        });
                    }
                };
                old_accounts.insert(*hashed_address, old_account);
            }
        }

        let mut storage_roots = BTreeMap::new();
        for hashed_address in &changed_addresses {
            let old_account = old_accounts
                .get(hashed_address)
                .expect("changed address was cached");
            let old_storage_root = old_account
                .as_ref()
                .map_or(EMPTY_ROOT_HASH, |account| account.storage_root);
            let storage_root = if let Some(storage) = post_state.storages.get(hashed_address) {
                let starts_empty =
                    storage.wiped || old_account.is_none() || old_storage_root == EMPTY_ROOT_HASH;
                let mut storage_trie = if starts_empty {
                    Some(RevealableSparseTrie::revealed_empty())
                } else {
                    self.sparse.take_storage_trie(hashed_address)
                };

                if !starts_empty {
                    let mut slots = storage.storage.keys().copied().collect::<Vec<_>>();
                    slots.sort_unstable();
                    for hashed_slot in slots {
                        let path = Nibbles::unpack(hashed_slot);
                        let value = if let Some(revealed) = storage_trie
                            .as_ref()
                            .and_then(RevealableSparseTrie::as_revealed_ref)
                        {
                            match revealed.find_leaf(&path, None) {
                                Ok(LeafLookup::Exists) => {
                                    Some(revealed.get_leaf_value(&path).cloned().ok_or(
                                        WitnessImportError::PostStateStorageProofIncomplete {
                                            hashed_address: *hashed_address,
                                            hashed_slot,
                                            proof_target: hashed_slot,
                                        },
                                    )?)
                                }
                                Ok(LeafLookup::NonExistent) => None,
                                Err(LeafLookupError::BlindedNode { .. }) => {
                                    verified_flat_trie_lookup(
                                        old_storage_root,
                                        path,
                                        &self.flat_nodes,
                                    )
                                    .map_err(|()| {
                                        WitnessImportError::PostStateStorageProofIncomplete {
                                            hashed_address: *hashed_address,
                                            hashed_slot,
                                            proof_target: hashed_slot,
                                        }
                                    })?
                                }
                                Err(LeafLookupError::ValueMismatch { .. }) => {
                                    return Err(
                                        WitnessImportError::PostStateStorageProofIncomplete {
                                            hashed_address: *hashed_address,
                                            hashed_slot,
                                            proof_target: hashed_slot,
                                        },
                                    );
                                }
                            }
                        } else {
                            verified_flat_trie_lookup(old_storage_root, path, &self.flat_nodes)
                                .map_err(|()| {
                                    WitnessImportError::PostStateStorageProofIncomplete {
                                        hashed_address: *hashed_address,
                                        hashed_slot,
                                        proof_target: hashed_slot,
                                    }
                                })?
                        };
                        if let Some(value) = value {
                            decode_post_state_storage_leaf(*hashed_address, hashed_slot, &value)?;
                        }
                    }
                }

                let (upserts, removals) = post_state_storage_batches(storage);
                let fallback_upserts = upserts.clone();
                let fallback_removals = removals.clone();
                let sparse_update = match storage_trie.as_mut() {
                    Some(storage_trie) => apply_post_state_batch(storage_trie, upserts)
                        .and_then(|()| apply_post_state_batch(storage_trie, removals)),
                    None => Err(PostStateBatchError::ProofRequired(B256::ZERO)),
                };
                match sparse_update {
                    Ok(()) => storage_trie
                        .as_mut()
                        .and_then(RevealableSparseTrie::root)
                        .ok_or(WitnessImportError::PostStateStorageProofIncomplete {
                            hashed_address: *hashed_address,
                            hashed_slot: B256::ZERO,
                            proof_target: *hashed_address,
                        })?,
                    Err(PostStateBatchError::ProofRequired(_)) => {
                        verified_flat_storage_post_state_root(
                            old_storage_root,
                            &self.flat_nodes,
                            *hashed_address,
                            fallback_upserts,
                            fallback_removals,
                        )?
                    }
                    Err(error @ PostStateBatchError::Sparse(_)) => {
                        return Err(map_storage_update_error(*hashed_address, error));
                    }
                }
            } else {
                old_storage_root
            };
            storage_roots.insert(*hashed_address, storage_root);
        }

        let mut account_upserts = B256Map::default();
        let mut account_removals = B256Map::default();
        for hashed_address in changed_addresses {
            let final_account = post_state.accounts.get(&hashed_address).copied().ok_or(
                WitnessImportError::PostStateMissingFinalAccount(hashed_address),
            )?;
            let storage_root = storage_roots[&hashed_address];
            let encoded = encode_post_state_account_leaf(final_account, storage_root);
            if encoded.is_empty() {
                account_removals.insert(hashed_address, LeafUpdate::Changed(Vec::new()));
            } else {
                account_upserts.insert(hashed_address, LeafUpdate::Changed(encoded));
            }
        }

        let fallback_upserts = account_upserts.clone();
        let fallback_removals = account_removals.clone();
        let sparse_update = apply_post_state_batch(self.sparse.trie_mut(), account_upserts)
            .and_then(|()| apply_post_state_batch(self.sparse.trie_mut(), account_removals));
        let actual = match sparse_update {
            Ok(()) => {
                self.sparse
                    .root()
                    .map_err(|error| WitnessImportError::PostStateSparseTrie {
                        scope: "account root",
                        reason: error.to_string(),
                    })?
            }
            Err(PostStateBatchError::ProofRequired(_)) => verified_flat_account_post_state_root(
                self.pre_state_root,
                &self.flat_nodes,
                fallback_upserts,
                fallback_removals,
            )?,
            Err(PostStateBatchError::Sparse(reason)) => {
                return Err(WitnessImportError::PostStateSparseTrie {
                    scope: "account update",
                    reason,
                });
            }
        };
        let expected = self.target_header.state_root;
        if actual != expected {
            return Err(WitnessImportError::PostStateRootMismatch { expected, actual });
        }
        Ok(actual)
    }
}

#[derive(Debug)]
enum PostStateBatchError {
    ProofRequired(B256),
    Sparse(String),
}

fn apply_post_state_batch(
    trie: &mut RevealableSparseTrie,
    mut updates: B256Map<LeafUpdate>,
) -> Result<(), PostStateBatchError> {
    if updates.is_empty() {
        return Ok(());
    }
    let mut proof_target = None;
    trie.update_leaves(&mut updates, |target, _| {
        proof_target.get_or_insert(target);
    })
    .map_err(|error| PostStateBatchError::Sparse(error.to_string()))?;
    if let Some(target) = proof_target {
        return Err(PostStateBatchError::ProofRequired(target));
    }
    if let Some(target) = updates.keys().min().copied() {
        return Err(PostStateBatchError::ProofRequired(target));
    }
    Ok(())
}

fn post_state_storage_batches(
    storage: &reth_trie_common::HashedStorage,
) -> (B256Map<LeafUpdate>, B256Map<LeafUpdate>) {
    let mut upserts = B256Map::default();
    let mut removals = B256Map::default();
    for (&hashed_slot, &value) in &storage.storage {
        if value.is_zero() {
            removals.insert(hashed_slot, LeafUpdate::Changed(Vec::new()));
        } else {
            upserts.insert(
                hashed_slot,
                LeafUpdate::Changed(alloy_rlp::encode_fixed_size(&value).to_vec()),
            );
        }
    }
    (upserts, removals)
}

fn encode_post_state_account_leaf(
    account: Option<reth_primitives_traits::Account>,
    storage_root: B256,
) -> Vec<u8> {
    if account.is_none_or(|account| account.is_empty()) && storage_root == EMPTY_ROOT_HASH {
        return Vec::new();
    }
    let mut encoded = Vec::new();
    account
        .unwrap_or_default()
        .into_trie_account(storage_root)
        .encode(&mut encoded);
    encoded
}

fn decode_post_state_account_leaf(
    hashed_address: B256,
    value: &[u8],
) -> Result<TrieAccount, WitnessImportError> {
    let mut input = value;
    let account = TrieAccount::decode(&mut input).map_err(|error| {
        WitnessImportError::PostStateInvalidAccountLeaf {
            hashed_address,
            reason: error.to_string(),
        }
    })?;
    if !input.is_empty() {
        return Err(WitnessImportError::PostStateInvalidAccountLeaf {
            hashed_address,
            reason: "trailing RLP bytes".to_string(),
        });
    }
    Ok(account)
}

fn decode_post_state_storage_leaf(
    hashed_address: B256,
    hashed_slot: B256,
    value: &[u8],
) -> Result<U256, WitnessImportError> {
    let mut input = value;
    let decoded = U256::decode(&mut input).map_err(|error| {
        WitnessImportError::PostStateInvalidStorageLeaf {
            hashed_address,
            hashed_slot,
            reason: error.to_string(),
        }
    })?;
    if !input.is_empty() {
        return Err(WitnessImportError::PostStateInvalidStorageLeaf {
            hashed_address,
            hashed_slot,
            reason: "trailing RLP bytes".to_string(),
        });
    }
    Ok(decoded)
}

fn map_storage_update_error(
    hashed_address: B256,
    error: PostStateBatchError,
) -> WitnessImportError {
    match error {
        PostStateBatchError::ProofRequired(proof_target) => {
            WitnessImportError::PostStateStorageProofIncomplete {
                hashed_address,
                hashed_slot: proof_target,
                proof_target,
            }
        }
        PostStateBatchError::Sparse(reason) => WitnessImportError::PostStateSparseTrie {
            scope: "storage update",
            reason,
        },
    }
}

impl Database for WitnessDb {
    type Error = WitnessImportError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.resolve_account(address)?;
        Ok(self.strict.basic(address)?)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.strict.code_by_hash(code_hash)?)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.resolve_storage(address, index)?;
        Ok(self.strict.storage(address, index)?)
    }

    fn storage_by_account_id(
        &mut self,
        address: Address,
        account_id: revm::state::AccountId,
        storage_key: U256,
    ) -> Result<U256, Self::Error> {
        self.resolve_storage(address, storage_key)?;
        // Trie accounts do not authenticate a provider-specific account ID.
        // Preserve StrictDb's address/ID/load binding instead of accepting an
        // arbitrary optimization hint after the proof lookup.
        Ok(self
            .strict
            .storage_by_account_id(address, account_id, storage_key)?)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.strict.block_hash(number)?)
    }
}

impl DBErrorMarker for WitnessImportError {}

fn validate_key_preimages(keys: &[Bytes]) -> Result<(), WitnessImportError> {
    for (index, key) in keys.iter().enumerate() {
        if !matches!(key.len(), 20 | 32) {
            return Err(WitnessImportError::InvalidKeyLength {
                index,
                length: key.len(),
            });
        }
    }
    Ok(())
}

fn decode_target_header(encoded: &[u8]) -> Result<Header, WitnessImportError> {
    let mut input = encoded;
    let header = Header::decode(&mut input)
        .map_err(|error| WitnessImportError::InvalidTargetHeader(error.to_string()))?;
    if !input.is_empty() {
        return Err(WitnessImportError::TargetHeaderTrailingBytes);
    }
    Ok(header)
}

type TargetBlock = ConsensusBlock<EthereumTxEnvelope<TxEip4844>>;

fn validate_target_block(
    encoded: &Bytes,
    target_header: &Header,
    target_block_hash: B256,
) -> Result<usize, WitnessImportError> {
    let mut after_prefix = encoded.as_ref();
    let rlp_header = alloy_rlp::Header::decode(&mut after_prefix)
        .map_err(|error| WitnessImportError::InvalidTargetBlock(error.to_string()))?;
    if !rlp_header.list {
        return Err(WitnessImportError::InvalidTargetBlock(
            "outer RLP item is not a list".to_string(),
        ));
    }
    let prefix_length = encoded.len() - after_prefix.len();
    let block_length = prefix_length
        .checked_add(rlp_header.payload_length)
        .ok_or_else(|| WitnessImportError::InvalidTargetBlock("RLP length overflow".to_string()))?;
    if block_length > encoded.len() {
        return Err(WitnessImportError::InvalidTargetBlock(
            "RLP payload is truncated".to_string(),
        ));
    }
    if block_length < encoded.len() {
        return Err(WitnessImportError::TargetBlockTrailingBytes);
    }

    let mut input = encoded.as_ref();
    let sealed = TargetBlock::decode_sealed(&mut input)
        .map_err(|error| WitnessImportError::InvalidTargetBlock(error.to_string()))?;
    if !input.is_empty() {
        return Err(WitnessImportError::TargetBlockTrailingBytes);
    }
    if alloy_rlp::encode(sealed.inner()).as_slice() != encoded.as_ref() {
        return Err(WitnessImportError::TargetBlockNonCanonical);
    }

    let block = sealed.inner();
    if &block.header != target_header {
        return Err(WitnessImportError::TargetBlockHeaderMismatch);
    }
    let actual_block_hash = sealed.hash();
    if actual_block_hash != target_block_hash {
        return Err(WitnessImportError::TargetRawBlockHashMismatch {
            expected: target_block_hash,
            actual: actual_block_hash,
        });
    }

    let actual_transactions_root = proofs::calculate_transaction_root(&block.body.transactions);
    if actual_transactions_root != block.header.transactions_root {
        return Err(WitnessImportError::TargetBlockTransactionsRootMismatch {
            expected: block.header.transactions_root,
            actual: actual_transactions_root,
        });
    }
    let actual_ommers_hash = block.body.calculate_ommers_root();
    if actual_ommers_hash != block.header.ommers_hash {
        return Err(WitnessImportError::TargetBlockOmmersHashMismatch {
            expected: block.header.ommers_hash,
            actual: actual_ommers_hash,
        });
    }
    let actual_withdrawals_root = block.body.calculate_withdrawals_root();
    if actual_withdrawals_root != block.header.withdrawals_root {
        return Err(WitnessImportError::TargetBlockWithdrawalsRootMismatch {
            expected: block.header.withdrawals_root,
            actual: actual_withdrawals_root,
        });
    }

    Ok(block.body.transactions.len())
}

fn decode_and_validate_headers(
    encoded: &[Bytes],
    target_block_number: u64,
    target_parent_hash: B256,
) -> Result<Vec<Header>, WitnessImportError> {
    if encoded.is_empty() {
        return Err(WitnessImportError::MissingHeaders);
    }

    let mut headers = Vec::with_capacity(encoded.len());
    for (index, bytes) in encoded.iter().enumerate() {
        let mut input = bytes.as_ref();
        let header =
            Header::decode(&mut input).map_err(|error| WitnessImportError::InvalidHeader {
                index,
                reason: error.to_string(),
            })?;
        if !input.is_empty() {
            return Err(WitnessImportError::HeaderTrailingBytes { index });
        }
        headers.push(header);
    }

    for (index, pair) in headers.windows(2).enumerate() {
        let older = &pair[0];
        let newer = &pair[1];
        let expected_number = older.number.saturating_add(1);
        if newer.number != expected_number {
            return Err(WitnessImportError::HeaderNumberDiscontinuity {
                index: index + 1,
                expected: expected_number,
                actual: newer.number,
            });
        }
        let expected_parent = older.hash_slow();
        if newer.parent_hash != expected_parent {
            return Err(WitnessImportError::HeaderParentMismatch {
                index: index + 1,
                expected: expected_parent,
                actual: newer.parent_hash,
            });
        }
    }

    let parent = headers.last().expect("non-empty checked");
    let expected_number = target_block_number - 1;
    if parent.number != expected_number {
        return Err(WitnessImportError::TargetParentNumberMismatch {
            expected: expected_number,
            actual: parent.number,
        });
    }
    let actual_hash = parent.hash_slow();
    if actual_hash != target_parent_hash {
        return Err(WitnessImportError::TargetParentHashMismatch {
            expected: target_parent_hash,
            actual: actual_hash,
        });
    }

    Ok(headers)
}

fn flat_node_store(state_nodes: &[Bytes]) -> B256Map<Bytes> {
    let mut nodes = B256Map::default();
    for node in state_nodes {
        nodes.entry(keccak256(node)).or_insert_with(|| node.clone());
    }
    nodes
}

fn reveal_sparse_trie(
    pre_state_root: B256,
    nodes: &B256Map<Bytes>,
) -> Result<SparseStateTrie, WitnessImportError> {
    if pre_state_root == EMPTY_ROOT_HASH {
        let sparse =
            SparseStateTrie::new().with_accounts_trie(RevealableSparseTrie::revealed_empty());
        return Ok(sparse);
    }

    if !nodes.contains_key(&pre_state_root) {
        return Err(WitnessImportError::MissingStateRoot(pre_state_root));
    }

    let proof = DecodedMultiProofV2::from_witness(pre_state_root, &nodes)
        .map_err(|error| WitnessImportError::InvalidMultiproof(error.to_string()))?;
    let mut sparse = SparseStateTrie::new();
    sparse
        .reveal_decoded_multiproof_v2(proof)
        .map_err(|error| WitnessImportError::InvalidSparseTrie(error.to_string()))?;
    let actual = sparse
        .root()
        .map_err(|error| WitnessImportError::InvalidSparseTrie(error.to_string()))?;
    if actual != pre_state_root {
        return Err(WitnessImportError::PreStateRootMismatch {
            expected: pre_state_root,
            actual,
        });
    }
    Ok(sparse)
}

fn decode_account(address: Address, value: &[u8]) -> Result<TrieAccount, WitnessImportError> {
    let mut input = value;
    let account = TrieAccount::decode(&mut input).map_err(|error| {
        WitnessImportError::InvalidAccountLeaf {
            address,
            reason: error.to_string(),
        }
    })?;
    if !input.is_empty() {
        return Err(WitnessImportError::AccountLeafTrailingBytes(address));
    }
    Ok(account)
}

fn resolve_storage(
    sparse: &SparseStateTrie,
    flat_nodes: &B256Map<Bytes>,
    target: &StorageAccess,
    hashed_address: B256,
    storage_root: B256,
) -> Result<U256, WitnessImportError> {
    let hashed_slot = keccak256(target.slot);
    let path = Nibbles::unpack(hashed_slot);
    let value = if let Some(storage_trie) = sparse.storage_trie_ref(&hashed_address) {
        match storage_trie.find_leaf(&path, None) {
            Ok(LeafLookup::Exists) => Some(storage_trie.get_leaf_value(&path).cloned().ok_or(
                WitnessImportError::IncompleteStorageProof {
                    address: target.address,
                    slot: target.slot,
                },
            )?),
            Ok(LeafLookup::NonExistent) => None,
            Err(LeafLookupError::BlindedNode { .. }) => {
                verified_flat_trie_lookup(storage_root, path, flat_nodes).map_err(|_| {
                    WitnessImportError::IncompleteStorageProof {
                        address: target.address,
                        slot: target.slot,
                    }
                })?
            }
            Err(LeafLookupError::ValueMismatch { .. }) => {
                return Err(WitnessImportError::IncompleteStorageProof {
                    address: target.address,
                    slot: target.slot,
                });
            }
        }
    } else {
        verified_flat_trie_lookup(storage_root, path, flat_nodes).map_err(|_| {
            WitnessImportError::IncompleteStorageProof {
                address: target.address,
                slot: target.slot,
            }
        })?
    };

    match value {
        Some(value) => {
            let mut input = value.as_slice();
            let decoded = U256::decode(&mut input).map_err(|error| {
                WitnessImportError::InvalidStorageLeaf {
                    address: target.address,
                    slot: target.slot,
                    reason: error.to_string(),
                }
            })?;
            if !input.is_empty() {
                return Err(WitnessImportError::StorageLeafTrailingBytes {
                    address: target.address,
                    slot: target.slot,
                });
            }
            Ok(decoded)
        }
        None => Ok(U256::ZERO),
    }
}

#[derive(Debug)]
enum FlatTrieStep {
    Hash(B256),
    Value(Option<Vec<u8>>),
}

fn verified_flat_trie_lookup(
    root: B256,
    key: Nibbles,
    flat_nodes: &B256Map<Bytes>,
) -> Result<Option<Vec<u8>>, ()> {
    if root == EMPTY_ROOT_HASH {
        verify_proof(root, key, None, std::iter::empty::<&Bytes>()).map_err(|_| ())?;
        return Ok(None);
    }

    let mut proof = Vec::new();
    let mut walked = Nibbles::new();
    let mut next_hash = root;
    let expected = loop {
        if proof.len() > key.len() {
            return Err(());
        }
        let node = flat_nodes.get(&next_hash).cloned().ok_or(())?;
        let decoded = decode_flat_trie_node(&node)?;
        proof.push(node);
        match walk_flat_trie_node(decoded, &key, &mut walked)? {
            FlatTrieStep::Hash(hash) => next_hash = hash,
            FlatTrieStep::Value(value) => break value,
        }
    };

    verify_proof(root, key, expected.clone(), proof.iter()).map_err(|_| ())?;
    Ok(expected)
}

fn decode_flat_trie_node(node: &[u8]) -> Result<TrieNode, ()> {
    let mut input = node;
    let decoded = TrieNode::decode(&mut input).map_err(|_| ())?;
    if input.is_empty() {
        Ok(decoded)
    } else {
        Err(())
    }
}

fn walk_flat_trie_node(
    node: TrieNode,
    key: &Nibbles,
    walked: &mut Nibbles,
) -> Result<FlatTrieStep, ()> {
    match node {
        TrieNode::EmptyRoot => Err(()),
        TrieNode::Leaf(leaf) => {
            let mut leaf_path = *walked;
            leaf_path.extend(&leaf.key);
            if leaf_path == *key {
                Ok(FlatTrieStep::Value(Some(leaf.value)))
            } else {
                Ok(FlatTrieStep::Value(None))
            }
        }
        TrieNode::Extension(extension) => {
            let remaining_end = walked.len().saturating_add(extension.key.len());
            if remaining_end > key.len() || key.slice(walked.len()..remaining_end) != extension.key
            {
                return Ok(FlatTrieStep::Value(None));
            }
            walked.extend(&extension.key);
            walk_flat_trie_child(extension.child, key, walked)
        }
        TrieNode::Branch(branch) => {
            let Some(nibble) = key.get(walked.len()) else {
                return Ok(FlatTrieStep::Value(None));
            };
            let Some(child) = flat_branch_child(&branch, nibble) else {
                return Ok(FlatTrieStep::Value(None));
            };
            walked.push_unchecked(nibble);
            walk_flat_trie_child(child, key, walked)
        }
    }
}

fn flat_branch_child(branch: &BranchNode, nibble: u8) -> Option<RlpNode> {
    let mut stack_index = 0;
    for index in CHILD_INDEX_RANGE {
        if branch.state_mask.is_bit_set(index) {
            if index == nibble {
                return branch.stack.get(stack_index).cloned();
            }
            stack_index += 1;
        }
    }
    None
}

fn walk_flat_trie_child(
    child: RlpNode,
    key: &Nibbles,
    walked: &mut Nibbles,
) -> Result<FlatTrieStep, ()> {
    if let Some(hash) = child.as_hash() {
        Ok(FlatTrieStep::Hash(hash))
    } else {
        walk_flat_trie_node(decode_flat_trie_node(child.as_slice())?, key, walked)
    }
}

/// A proof-backed partial MPT. Unknown subtries remain opaque hashes, while
/// nodes reachable through witness hashes are decoded just far enough to
/// apply proven leaf updates.
#[derive(Debug)]
enum PartialTrieNode {
    Empty,
    Hash(B256),
    Leaf { key: Nibbles, value: Vec<u8> },
    Extension { key: Nibbles, child: Box<Self> },
    Branch(Box<[Option<Box<Self>>; 16]>),
}

impl PartialTrieNode {
    fn from_root(root: B256, nodes: &B256Map<Bytes>) -> Result<Self, String> {
        if root == EMPTY_ROOT_HASH {
            return Ok(Self::Empty);
        }
        let encoded = nodes
            .get(&root)
            .ok_or_else(|| format!("missing root node {root}"))?;
        if keccak256(encoded) != root {
            return Err(format!("root node hash mismatch for {root}"));
        }
        Self::from_encoded(encoded, nodes, 0)
    }

    fn from_encoded(encoded: &[u8], nodes: &B256Map<Bytes>, depth: usize) -> Result<Self, String> {
        if depth > 128 {
            return Err("partial trie exceeds the fixed-key MPT depth bound".to_string());
        }
        let decoded = decode_flat_trie_node(encoded)
            .map_err(|()| "invalid partial trie node RLP".to_string())?;
        match decoded {
            TrieNode::EmptyRoot => Ok(Self::Empty),
            TrieNode::Leaf(leaf) => Ok(Self::Leaf {
                key: leaf.key,
                value: leaf.value,
            }),
            TrieNode::Extension(extension) => Ok(Self::Extension {
                key: extension.key,
                child: Box::new(Self::from_child(extension.child, nodes, depth + 1)?),
            }),
            TrieNode::Branch(branch) => {
                let mut children = Box::new(std::array::from_fn(|_| None));
                let mut stack_index = 0;
                for nibble in CHILD_INDEX_RANGE {
                    if branch.state_mask.is_bit_set(nibble) {
                        let child = branch
                            .stack
                            .get(stack_index)
                            .cloned()
                            .ok_or_else(|| "branch child stack is incomplete".to_string())?;
                        children[nibble as usize] =
                            Some(Box::new(Self::from_child(child, nodes, depth + 1)?));
                        stack_index += 1;
                    }
                }
                if stack_index != branch.stack.len() {
                    return Err("branch child stack has trailing entries".to_string());
                }
                Ok(Self::Branch(children))
            }
        }
    }

    fn from_child(child: RlpNode, nodes: &B256Map<Bytes>, depth: usize) -> Result<Self, String> {
        if let Some(hash) = child.as_hash() {
            return match nodes.get(&hash) {
                Some(encoded) => {
                    if keccak256(encoded) != hash {
                        return Err(format!("child node hash mismatch for {hash}"));
                    }
                    Self::from_encoded(encoded, nodes, depth)
                }
                None => Ok(Self::Hash(hash)),
            };
        }
        Self::from_encoded(child.as_slice(), nodes, depth)
    }

    fn upsert(self, path: Nibbles, value: Vec<u8>) -> Result<Self, ()> {
        match self {
            Self::Empty => Ok(Self::Leaf { key: path, value }),
            Self::Hash(_) => Err(()),
            Self::Leaf {
                key: old_key,
                value: old_value,
            } => {
                if old_key == path {
                    return Ok(Self::Leaf {
                        key: old_key,
                        value,
                    });
                }
                let common = old_key.common_prefix_length(&path);
                let old_nibble = old_key.get(common).ok_or(())?;
                let new_nibble = path.get(common).ok_or(())?;
                if old_nibble == new_nibble {
                    return Err(());
                }
                let mut children = Box::new(std::array::from_fn(|_| None));
                children[old_nibble as usize] = Some(Box::new(Self::Leaf {
                    key: old_key.slice(common + 1..),
                    value: old_value,
                }));
                children[new_nibble as usize] = Some(Box::new(Self::Leaf {
                    key: path.slice(common + 1..),
                    value,
                }));
                Ok(Self::with_prefix(
                    old_key.slice(..common),
                    Self::Branch(children),
                ))
            }
            Self::Extension { key, child } => {
                let common = key.common_prefix_length(&path);
                if common == key.len() {
                    let child = child.upsert(path.slice(common..), value)?;
                    return Ok(Self::with_prefix(key, child));
                }
                let old_nibble = key.get(common).ok_or(())?;
                let new_nibble = path.get(common).ok_or(())?;
                if old_nibble == new_nibble {
                    return Err(());
                }
                let old_child = Self::with_prefix(key.slice(common + 1..), *child);
                let new_child = Self::Leaf {
                    key: path.slice(common + 1..),
                    value,
                };
                let mut children = Box::new(std::array::from_fn(|_| None));
                children[old_nibble as usize] = Some(Box::new(old_child));
                children[new_nibble as usize] = Some(Box::new(new_child));
                Ok(Self::with_prefix(
                    key.slice(..common),
                    Self::Branch(children),
                ))
            }
            Self::Branch(mut children) => {
                let nibble = path.first().ok_or(())?;
                let child = children[nibble as usize].take().map_or(
                    Ok(Self::Leaf {
                        key: path.slice(1..),
                        value: value.clone(),
                    }),
                    |child| child.upsert(path.slice(1..), value),
                )?;
                children[nibble as usize] = Some(Box::new(child));
                Ok(Self::Branch(children))
            }
        }
    }

    fn remove(self, path: Nibbles) -> Result<Self, ()> {
        match self {
            Self::Empty => Ok(Self::Empty),
            Self::Hash(_) => Err(()),
            Self::Leaf { key, value } => {
                if key == path {
                    Ok(Self::Empty)
                } else {
                    Ok(Self::Leaf { key, value })
                }
            }
            Self::Extension { key, child } => {
                if path.len() < key.len() || path.slice(..key.len()) != key {
                    return Ok(Self::Extension { key, child });
                }
                let child = child.remove(path.slice(key.len()..))?;
                Ok(Self::with_prefix(key, child))
            }
            Self::Branch(mut children) => {
                let Some(nibble) = path.first() else {
                    return Ok(Self::Branch(children));
                };
                let Some(child) = children[nibble as usize].take() else {
                    return Ok(Self::Branch(children));
                };
                let child = child.remove(path.slice(1..))?;
                if !matches!(child, Self::Empty) {
                    children[nibble as usize] = Some(Box::new(child));
                }
                Self::normalize_branch(children)
            }
        }
    }

    fn with_prefix(prefix: Nibbles, child: Self) -> Self {
        if prefix.is_empty() {
            return child;
        }
        match child {
            Self::Empty => Self::Empty,
            Self::Leaf { key, value } => {
                let mut combined = prefix;
                combined.extend(&key);
                Self::Leaf {
                    key: combined,
                    value,
                }
            }
            Self::Extension { key, child } => {
                let mut combined = prefix;
                combined.extend(&key);
                Self::Extension {
                    key: combined,
                    child,
                }
            }
            child => Self::Extension {
                key: prefix,
                child: Box::new(child),
            },
        }
    }

    fn normalize_branch(mut children: Box<[Option<Box<Self>>; 16]>) -> Result<Self, ()> {
        let mut only_child = None;
        for nibble in CHILD_INDEX_RANGE {
            if children[nibble as usize].is_some() {
                if only_child.is_some() {
                    return Ok(Self::Branch(children));
                }
                only_child = Some(nibble);
            }
        }
        let Some(nibble) = only_child else {
            return Ok(Self::Empty);
        };
        let child = *children[nibble as usize]
            .take()
            .expect("single branch child was counted");
        if matches!(child, Self::Hash(_)) {
            // Collapsing a one-child branch requires the child's node shape:
            // a leaf absorbs the nibble into its compact key, while a branch
            // or extension needs a new/merged extension. A hash alone does
            // not authenticate which canonical encoding is required.
            return Err(());
        }
        let mut prefix = Nibbles::new();
        prefix.push_unchecked(nibble);
        Ok(Self::with_prefix(prefix, child))
    }

    fn encoded(&self) -> Result<Vec<u8>, String> {
        match self {
            Self::Empty => Ok(vec![alloy_rlp::EMPTY_STRING_CODE]),
            Self::Hash(hash) => Err(format!("cannot encode opaque root hash {hash}")),
            Self::Leaf { key, value } => Ok(alloy_rlp::encode(LeafNodeRef::new(key, value))),
            Self::Extension { key, child } => {
                let child = child.rlp_node()?;
                Ok(alloy_rlp::encode(ExtensionNodeRef::new(
                    key,
                    child.as_slice(),
                )))
            }
            Self::Branch(children) => {
                let mut stack = Vec::new();
                let mut mask = 0u16;
                for nibble in CHILD_INDEX_RANGE {
                    if let Some(child) = &children[nibble as usize] {
                        stack.push(child.rlp_node()?);
                        mask |= 1u16 << nibble;
                    }
                }
                if stack.len() < 2 {
                    return Err("non-canonical partial trie branch".to_string());
                }
                Ok(alloy_rlp::encode(BranchNodeRef::new(
                    &stack,
                    TrieMask::new(mask),
                )))
            }
        }
    }

    fn rlp_node(&self) -> Result<RlpNode, String> {
        match self {
            Self::Hash(hash) => Ok(RlpNode::word_rlp(hash)),
            _ => Ok(RlpNode::from_rlp(&self.encoded()?)),
        }
    }

    fn root(&self) -> Result<B256, String> {
        match self {
            Self::Empty => Ok(EMPTY_ROOT_HASH),
            Self::Hash(hash) => Ok(*hash),
            _ => Ok(keccak256(self.encoded()?)),
        }
    }
}

#[derive(Debug)]
enum FlatPostStateError {
    ProofRequired(B256),
    Invalid(String),
    RootMismatch { expected: B256, actual: B256 },
}

fn verified_flat_trie_post_state_root(
    pre_state_root: B256,
    flat_nodes: &B256Map<Bytes>,
    upserts: B256Map<LeafUpdate>,
    removals: B256Map<LeafUpdate>,
) -> Result<B256, FlatPostStateError> {
    let mut updates = upserts.into_iter().collect::<Vec<_>>();
    updates.sort_unstable_by_key(|(key, _)| *key);
    let mut removals = removals.into_iter().collect::<Vec<_>>();
    removals.sort_unstable_by_key(|(key, _)| *key);
    updates.extend(removals);

    // Authenticate every original lookup independently before using the flat
    // node graph for a mutation. This keeps extra, unreferenced witness nodes
    // from authorizing a write.
    for (key, _) in &updates {
        verified_flat_trie_lookup(pre_state_root, Nibbles::unpack(*key), flat_nodes)
            .map_err(|()| FlatPostStateError::ProofRequired(*key))?;
    }

    let mut trie = PartialTrieNode::from_root(pre_state_root, flat_nodes)
        .map_err(FlatPostStateError::Invalid)?;
    let anchored_root = trie.root().map_err(FlatPostStateError::Invalid)?;
    if anchored_root != pre_state_root {
        return Err(FlatPostStateError::RootMismatch {
            expected: pre_state_root,
            actual: anchored_root,
        });
    }

    for (key, update) in updates {
        let path = Nibbles::unpack(key);
        trie = match update {
            LeafUpdate::Changed(value) if value.is_empty() => trie.remove(path),
            LeafUpdate::Changed(value) => trie.upsert(path, value),
            LeafUpdate::Touched => Err(()),
        }
        .map_err(|()| FlatPostStateError::ProofRequired(key))?;
    }

    trie.root().map_err(FlatPostStateError::Invalid)
}

fn verified_flat_account_post_state_root(
    pre_state_root: B256,
    flat_nodes: &B256Map<Bytes>,
    upserts: B256Map<LeafUpdate>,
    removals: B256Map<LeafUpdate>,
) -> Result<B256, WitnessImportError> {
    verified_flat_trie_post_state_root(pre_state_root, flat_nodes, upserts, removals).map_err(
        |error| match error {
            FlatPostStateError::ProofRequired(proof_target) => {
                WitnessImportError::PostStateAccountProofIncomplete {
                    hashed_address: proof_target,
                    proof_target,
                }
            }
            FlatPostStateError::Invalid(reason) => WitnessImportError::PostStateSparseTrie {
                scope: "account flat update",
                reason,
            },
            FlatPostStateError::RootMismatch { expected, actual } => {
                WitnessImportError::PreStateRootMismatch { expected, actual }
            }
        },
    )
}

fn verified_flat_storage_post_state_root(
    pre_storage_root: B256,
    flat_nodes: &B256Map<Bytes>,
    hashed_address: B256,
    upserts: B256Map<LeafUpdate>,
    removals: B256Map<LeafUpdate>,
) -> Result<B256, WitnessImportError> {
    verified_flat_trie_post_state_root(pre_storage_root, flat_nodes, upserts, removals).map_err(
        |error| match error {
            FlatPostStateError::ProofRequired(proof_target) => {
                WitnessImportError::PostStateStorageProofIncomplete {
                    hashed_address,
                    hashed_slot: proof_target,
                    proof_target,
                }
            }
            FlatPostStateError::Invalid(reason) => WitnessImportError::PostStateSparseTrie {
                scope: "storage flat update",
                reason,
            },
            FlatPostStateError::RootMismatch { expected, actual } => {
                WitnessImportError::PreStateRootMismatch { expected, actual }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use alloy_trie::TrieMask;
    use reth_trie_common::{BranchNodeV2, ExtensionNode, LeafNode, RlpNode, TrieNodeV2};
    use revm::database::{states::StorageSlot, AccountStatus, BundleAccount};
    use revm::state::AccountId;

    const ACCOUNT: Address = address!("1111111111111111111111111111111111111111");
    const ABSENT: Address = address!("2222222222222222222222222222222222222222");
    const OUTSIDE: Address = address!("3333333333333333333333333333333333333333");

    fn fixture() -> WitnessBundle {
        let slot = B256::ZERO;
        let hidden_slot = B256::from(U256::from(1).to_be_bytes::<32>());
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let storage_value = alloy_rlp::encode(U256::from(42));
        let storage_path = Nibbles::unpack(keccak256(slot));
        let storage_node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            storage_path.slice(1..),
            storage_value,
        )));
        let hidden_storage_path = Nibbles::unpack(keccak256(hidden_slot));
        let hidden_storage_node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            hidden_storage_path.slice(1..),
            alloy_rlp::encode(U256::from(99)),
        )));
        let storage_root_node = branch_node(
            (storage_path.first().unwrap(), &storage_node),
            (hidden_storage_path.first().unwrap(), &hidden_storage_node),
        );
        let storage_root = keccak256(&storage_root_node);
        let account = TrieAccount {
            nonce: 7,
            balance: U256::from(1_000_000),
            storage_root,
            code_hash: keccak256(&code),
        };
        let account_path = Nibbles::unpack(keccak256(ACCOUNT));
        let account_node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            account_path.slice(1..),
            alloy_rlp::encode(account),
        )));
        let outside_path = Nibbles::unpack(keccak256(OUTSIDE));
        let outside_node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            outside_path.slice(1..),
            alloy_rlp::encode(TrieAccount::default()),
        )));
        let state_root_node = branch_node(
            (account_path.first().unwrap(), &account_node),
            (outside_path.first().unwrap(), &outside_node),
        );
        let state_root = keccak256(&state_root_node);
        let parent = Header {
            number: 99,
            state_root,
            ..Default::default()
        };
        let target = Header {
            number: 100,
            parent_hash: parent.hash_slow(),
            ..Default::default()
        };

        WitnessBundle {
            target_header: alloy_rlp::encode(&target).into(),
            target_block_hash: target.hash_slow(),
            target_block: None,
            witness: ExecutionWitness {
                state: vec![
                    state_root_node.into(),
                    account_node.into(),
                    storage_root_node.into(),
                    storage_node.into(),
                ],
                codes: vec![code],
                keys: vec![
                    Bytes::copy_from_slice(ACCOUNT.as_slice()),
                    slot.to_vec().into(),
                ],
                headers: vec![alloy_rlp::encode(parent).into()],
            },
            access_manifest: AccessManifest::default(),
        }
    }

    fn branch_node(first: (u8, &[u8]), second: (u8, &[u8])) -> Vec<u8> {
        assert_ne!(first.0, second.0, "fixture paths must branch at the root");
        let mut children = [first, second];
        children.sort_unstable_by_key(|child| child.0);
        let stack = children
            .iter()
            .map(|(_, node)| RlpNode::from_rlp(node))
            .collect();
        let state_mask = TrieMask::new((1u16 << first.0) | (1u16 << second.0));
        alloy_rlp::encode(TrieNodeV2::Branch(BranchNodeV2::new(
            Nibbles::default(),
            stack,
            state_mask,
            None,
        )))
    }

    fn terminal_extension_exclusion_fixture() -> (WitnessBundle, U256) {
        let ((first_slot, first_path), (second_slot, second_path)) = slots_with_shared_prefix();
        let shared_len = shared_prefix_len(&first_path, &second_path);
        assert!(shared_len > 1);
        assert_ne!(
            first_path.get(shared_len),
            second_path.get(shared_len),
            "fixture storage leaves must diverge after the extension"
        );

        let first_value = U256::from(7);
        let second_value = U256::from(9);
        let first_leaf = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            first_path.slice(shared_len + 1..),
            alloy_rlp::encode_fixed_size(&first_value).to_vec(),
        )));
        let second_leaf = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            second_path.slice(shared_len + 1..),
            alloy_rlp::encode_fixed_size(&second_value).to_vec(),
        )));
        let branch = branch_node(
            (first_path.get(shared_len).unwrap(), &first_leaf),
            (second_path.get(shared_len).unwrap(), &second_leaf),
        );
        let branch_ref = RlpNode::from_rlp(&branch);
        assert!(branch_ref.is_hash());
        let extension_key = first_path.slice(1..shared_len);
        let terminal_extension = alloy_rlp::encode(TrieNodeV2::Extension(ExtensionNode::new(
            extension_key,
            branch_ref,
        )));

        let (sibling_slot, sibling_path) = (0u64..)
            .map(U256::from)
            .map(|slot| {
                let path = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes::<32>())));
                (slot, path)
            })
            .find(|(_, path)| path.first() != first_path.first())
            .expect("find a sibling root branch");
        let sibling_value = U256::from(11);
        let sibling_leaf = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            sibling_path.slice(1..),
            alloy_rlp::encode_fixed_size(&sibling_value).to_vec(),
        )));
        let storage_root_node = branch_node(
            (first_path.first().unwrap(), &terminal_extension),
            (sibling_path.first().unwrap(), &sibling_leaf),
        );
        let storage_root = keccak256(&storage_root_node);
        assert_eq!(
            storage_root,
            alloy_trie::root::storage_root_unhashed([
                (B256::from(first_slot.to_be_bytes::<32>()), first_value,),
                (B256::from(second_slot.to_be_bytes::<32>()), second_value,),
                (B256::from(sibling_slot.to_be_bytes::<32>()), sibling_value,),
            ])
        );

        let target_slot = (0u64..)
            .map(U256::from)
            .find(|slot| {
                let path = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes::<32>())));
                path.first() == first_path.first()
                    && path.slice(1..shared_len) != first_path.slice(1..shared_len)
            })
            .expect("find target that diverges inside the terminal extension");
        let target_key = B256::from(target_slot.to_be_bytes::<32>());

        let account = TrieAccount {
            nonce: 1,
            balance: U256::ZERO,
            storage_root,
            code_hash: KECCAK_EMPTY,
        };
        let account_path = Nibbles::unpack(keccak256(ACCOUNT));
        let account_node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            account_path,
            alloy_rlp::encode(account),
        )));
        let state_root = keccak256(&account_node);
        let final_storage_root = alloy_trie::root::storage_root_unhashed([
            (B256::from(first_slot.to_be_bytes::<32>()), first_value),
            (B256::from(second_slot.to_be_bytes::<32>()), second_value),
            (B256::from(sibling_slot.to_be_bytes::<32>()), sibling_value),
            (target_key, U256::from(5)),
        ]);
        let final_state_root = alloy_trie::root::state_root_unhashed([(
            ACCOUNT,
            TrieAccount {
                storage_root: final_storage_root,
                ..account
            },
        )]);
        let parent = Header {
            number: 99,
            state_root,
            ..Default::default()
        };
        let target = Header {
            number: 100,
            parent_hash: parent.hash_slow(),
            state_root: final_state_root,
            ..Default::default()
        };

        (
            WitnessBundle {
                target_header: alloy_rlp::encode(&target).into(),
                target_block_hash: target.hash_slow(),
                target_block: None,
                witness: ExecutionWitness {
                    state: vec![
                        account_node.into(),
                        storage_root_node.into(),
                        terminal_extension.into(),
                    ],
                    codes: Vec::new(),
                    keys: vec![
                        Bytes::copy_from_slice(ACCOUNT.as_slice()),
                        Bytes::copy_from_slice(target_key.as_slice()),
                    ],
                    headers: vec![alloy_rlp::encode(parent).into()],
                },
                access_manifest: AccessManifest::default(),
            },
            target_slot,
        )
    }

    fn terminal_extension_account_exclusion_fixture() -> (WitnessBundle, Address) {
        let ((first, first_path), (second, second_path)) = addresses_with_shared_prefix();
        let shared_len = shared_prefix_len(&first_path, &second_path);
        assert!(shared_len > 1);

        let first_account = TrieAccount {
            nonce: 1,
            balance: U256::from(10),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: KECCAK_EMPTY,
        };
        let second_account = TrieAccount {
            nonce: 2,
            balance: U256::from(20),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: KECCAK_EMPTY,
        };
        let first_leaf = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            first_path.slice(shared_len + 1..),
            alloy_rlp::encode(first_account),
        )));
        let second_leaf = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            second_path.slice(shared_len + 1..),
            alloy_rlp::encode(second_account),
        )));
        let branch = branch_node(
            (first_path.get(shared_len).unwrap(), &first_leaf),
            (second_path.get(shared_len).unwrap(), &second_leaf),
        );
        let terminal_extension = alloy_rlp::encode(TrieNodeV2::Extension(ExtensionNode::new(
            first_path.slice(1..shared_len),
            RlpNode::from_rlp(&branch),
        )));

        let (sibling, sibling_path) = (0u64..)
            .map(test_address)
            .map(|address| {
                let path = Nibbles::unpack(keccak256(address));
                (address, path)
            })
            .find(|(_, path)| path.first() != first_path.first())
            .expect("find account under a sibling root branch");
        let sibling_account = TrieAccount {
            nonce: 3,
            balance: U256::from(30),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: KECCAK_EMPTY,
        };
        let sibling_leaf = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            sibling_path.slice(1..),
            alloy_rlp::encode(sibling_account),
        )));
        let state_root_node = branch_node(
            (first_path.first().unwrap(), &terminal_extension),
            (sibling_path.first().unwrap(), &sibling_leaf),
        );
        let state_root = keccak256(&state_root_node);
        assert_eq!(
            state_root,
            alloy_trie::root::state_root_unhashed([
                (first, first_account),
                (second, second_account),
                (sibling, sibling_account),
            ])
        );

        let target = (0u64..)
            .map(test_address)
            .find(|address| {
                let path = Nibbles::unpack(keccak256(address));
                path.first() == first_path.first()
                    && path.slice(1..shared_len) != first_path.slice(1..shared_len)
            })
            .expect("find account that diverges inside the terminal extension");
        let target_account = TrieAccount {
            nonce: 4,
            balance: U256::from(40),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: KECCAK_EMPTY,
        };
        let final_state_root = alloy_trie::root::state_root_unhashed([
            (first, first_account),
            (second, second_account),
            (sibling, sibling_account),
            (target, target_account),
        ]);

        let parent = Header {
            number: 99,
            state_root,
            ..Default::default()
        };
        let target_header = Header {
            number: 100,
            parent_hash: parent.hash_slow(),
            state_root: final_state_root,
            ..Default::default()
        };
        (
            WitnessBundle {
                target_header: alloy_rlp::encode(&target_header).into(),
                target_block_hash: target_header.hash_slow(),
                target_block: None,
                witness: ExecutionWitness {
                    // The child branch is intentionally absent. The terminal
                    // extension alone proves that `target` is absent.
                    state: vec![state_root_node.into(), terminal_extension.into()],
                    codes: Vec::new(),
                    keys: vec![Bytes::copy_from_slice(target.as_slice())],
                    headers: vec![alloy_rlp::encode(parent).into()],
                },
                access_manifest: AccessManifest::default(),
            },
            target,
        )
    }

    fn addresses_with_shared_prefix() -> ((Address, Nibbles), (Address, Nibbles)) {
        let mut by_prefix = BTreeMap::<(u8, u8), Vec<(Address, Nibbles)>>::new();
        for value in 0u64.. {
            let address = test_address(value);
            let path = Nibbles::unpack(keccak256(address));
            let candidates = by_prefix
                .entry((path.first().unwrap(), path.get(1).unwrap()))
                .or_default();
            if let Some((other_address, other_path)) = candidates
                .iter()
                .find(|(_, other_path)| *other_path != path)
            {
                return ((*other_address, *other_path), (address, path));
            }
            candidates.push((address, path));
        }
        unreachable!("hashed account addresses must eventually share a prefix")
    }

    fn test_address(value: u64) -> Address {
        let mut bytes = [0u8; 20];
        bytes[12..].copy_from_slice(&value.to_be_bytes());
        Address::from(bytes)
    }

    fn slots_with_shared_prefix() -> ((U256, Nibbles), (U256, Nibbles)) {
        let mut by_prefix = BTreeMap::<(u8, u8), Vec<(U256, Nibbles)>>::new();
        for value in 0u64.. {
            let slot = U256::from(value);
            let path = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes::<32>())));
            let candidates = by_prefix
                .entry((path.first().unwrap(), path.get(1).unwrap()))
                .or_default();
            if let Some((other_slot, other_path)) = candidates
                .iter()
                .find(|(_, other_path)| *other_path != path)
            {
                return ((*other_slot, *other_path), (slot, path));
            }
            candidates.push((slot, path));
        }
        unreachable!("hashed storage slots must eventually share a prefix")
    }

    fn shared_prefix_len(first: &Nibbles, second: &Nibbles) -> usize {
        (0..first.len())
            .find(|&index| first.get(index) != second.get(index))
            .expect("distinct fixed-length keys must diverge")
    }

    #[test]
    fn terminal_extension_storage_exclusion_uses_authenticated_flat_path() {
        let (fixture, target_slot) = terminal_extension_exclusion_fixture();
        let target_path = Nibbles::unpack(keccak256(B256::from(target_slot.to_be_bytes::<32>())));
        let mut db = WitnessDb::from_bundle(fixture).expect("terminal extension witness");
        let storage_trie = db
            .sparse
            .storage_trie_ref(&keccak256(ACCOUNT))
            .expect("storage trie with terminal extension");
        assert!(matches!(
            storage_trie.find_leaf(&target_path, None),
            Err(LeafLookupError::BlindedNode { .. })
        ));

        assert_eq!(
            db.storage(ACCOUNT, target_slot)
                .expect("authenticated terminal-extension exclusion"),
            U256::ZERO
        );
    }

    #[test]
    fn missing_terminal_extension_node_fails_closed() {
        let (mut fixture, target_slot) = terminal_extension_exclusion_fixture();
        fixture.witness.state.remove(2);
        let mut db = WitnessDb::from_bundle(fixture).expect("account proof remains authenticated");

        assert!(matches!(
            db.storage(ACCOUNT, target_slot),
            Err(WitnessImportError::IncompleteStorageProof { address, .. })
                if address == ACCOUNT
        ));
    }

    #[test]
    fn tampered_terminal_extension_node_fails_closed() {
        let (mut fixture, target_slot) = terminal_extension_exclusion_fixture();
        let mut tampered = fixture.witness.state[2].to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        fixture.witness.state[2] = tampered.into();
        let mut db = WitnessDb::from_bundle(fixture).expect("account proof remains authenticated");

        assert!(matches!(
            db.storage(ACCOUNT, target_slot),
            Err(WitnessImportError::IncompleteStorageProof { address, .. })
                if address == ACCOUNT
        ));
    }

    #[test]
    fn terminal_extension_storage_exclusion_authorizes_exact_post_state_insert() {
        let (fixture, target_slot) = terminal_extension_exclusion_fixture();
        let expected = fixture_target_header(&fixture).state_root;
        let bundle = post_state_bundle(
            Some(account_info(1, 0)),
            Some(account_info(1, 0)),
            AccountStatus::Changed,
            &[(target_slot, U256::ZERO, U256::from(5))],
        );
        let mut db = WitnessDb::from_bundle(fixture).expect("terminal extension witness");
        assert_eq!(
            db.storage(ACCOUNT, target_slot)
                .expect("authenticated exclusion read"),
            U256::ZERO
        );

        assert_eq!(
            db.into_verified_post_state_root(&bundle)
                .expect("insert storage through authenticated extension exclusion"),
            expected
        );
    }

    #[test]
    fn terminal_extension_account_exclusion_authorizes_exact_post_state_insert() {
        let (fixture, target) = terminal_extension_account_exclusion_fixture();
        let expected = fixture_target_header(&fixture).state_root;
        let target_path = Nibbles::unpack(keccak256(target));
        let account = BundleAccount::new(
            None,
            Some(account_info(4, 40)),
            Default::default(),
            AccountStatus::Changed,
        );
        let bundle = BundleState {
            state: [(target, account)].into_iter().collect(),
            ..Default::default()
        };
        let db = WitnessDb::from_bundle(fixture).expect("terminal extension account witness");
        assert!(matches!(
            db.sparse
                .state_trie_ref()
                .expect("state trie")
                .find_leaf(&target_path, None),
            Err(LeafLookupError::BlindedNode { .. })
        ));

        assert_eq!(
            db.into_verified_post_state_root(&bundle)
                .expect("insert account through authenticated extension exclusion"),
            expected
        );
    }

    #[test]
    fn missing_terminal_extension_account_node_fails_post_state_insert() {
        let (mut fixture, target) = terminal_extension_account_exclusion_fixture();
        fixture.witness.state.remove(1);
        let account = BundleAccount::new(
            None,
            Some(account_info(4, 40)),
            Default::default(),
            AccountStatus::Changed,
        );
        let bundle = BundleState {
            state: [(target, account)].into_iter().collect(),
            ..Default::default()
        };
        let db = WitnessDb::from_bundle(fixture).expect("root remains authenticated");

        assert!(matches!(
            db.into_verified_post_state_root(&bundle),
            Err(WitnessImportError::PostStateAccountProofIncomplete {
                hashed_address,
                ..
            }) if hashed_address == keccak256(target)
        ));
    }

    #[test]
    fn tampered_terminal_extension_account_node_fails_post_state_insert() {
        let (mut fixture, target) = terminal_extension_account_exclusion_fixture();
        let mut tampered = fixture.witness.state[1].to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        fixture.witness.state[1] = tampered.into();
        let account = BundleAccount::new(
            None,
            Some(account_info(4, 40)),
            Default::default(),
            AccountStatus::Changed,
        );
        let bundle = BundleState {
            state: [(target, account)].into_iter().collect(),
            ..Default::default()
        };
        let db = WitnessDb::from_bundle(fixture).expect("root remains authenticated");

        assert!(matches!(
            db.into_verified_post_state_root(&bundle),
            Err(WitnessImportError::PostStateAccountProofIncomplete {
                hashed_address,
                ..
            }) if hashed_address == keccak256(target)
        ));
    }

    #[test]
    fn empty_noop_post_state_root_is_verified() {
        let parent = Header {
            number: 99,
            state_root: EMPTY_ROOT_HASH,
            ..Default::default()
        };
        let target = Header {
            number: 100,
            parent_hash: parent.hash_slow(),
            state_root: EMPTY_ROOT_HASH,
            ..Default::default()
        };
        let bundle = WitnessBundle {
            target_header: alloy_rlp::encode(&target).into(),
            target_block_hash: target.hash_slow(),
            target_block: None,
            witness: ExecutionWitness {
                state: Vec::new(),
                codes: Vec::new(),
                keys: Vec::new(),
                headers: vec![alloy_rlp::encode(parent).into()],
            },
            access_manifest: AccessManifest::default(),
        };
        let db = WitnessDb::from_bundle(bundle).expect("empty verified witness");

        assert_eq!(
            db.into_verified_post_state_root(&BundleState::default())
                .expect("empty post-state root"),
            EMPTY_ROOT_HASH
        );
    }

    #[test]
    fn account_only_update_preserves_nonempty_old_storage_root() {
        let slot = U256::from(1);
        let initial_storage = [(slot, U256::from(42))];
        let fixture = post_state_fixture(
            (7, U256::from(100)),
            &initial_storage,
            Some((8, U256::from(200))),
            &initial_storage,
            &[],
            false,
        );
        let bundle = post_state_bundle(
            Some(account_info(7, 100)),
            Some(account_info(8, 200)),
            AccountStatus::Changed,
            &[],
        );
        let expected = fixture_target_header(&fixture).state_root;
        let db = WitnessDb::from_bundle(fixture).expect("verified account witness");

        assert_eq!(
            db.into_verified_post_state_root(&bundle)
                .expect("account-only post-state root"),
            expected
        );
    }

    #[test]
    fn first_storage_insert_starts_from_authenticated_empty_root() {
        let slot = U256::from(3);
        let final_storage = [(slot, U256::from(9))];
        let fixture = post_state_fixture(
            (1, U256::from(10)),
            &[],
            Some((1, U256::from(10))),
            &final_storage,
            &[],
            false,
        );
        let bundle = post_state_bundle(
            Some(account_info(1, 10)),
            Some(account_info(1, 10)),
            AccountStatus::Changed,
            &[(slot, U256::ZERO, U256::from(9))],
        );
        let expected = fixture_target_header(&fixture).state_root;
        let db = WitnessDb::from_bundle(fixture).expect("verified empty-storage witness");

        assert_eq!(
            db.into_verified_post_state_root(&bundle)
                .expect("first storage insert"),
            expected
        );
    }

    #[test]
    fn absent_account_can_be_created_with_first_storage_insert() {
        let slot = U256::from(4);
        let final_storage = [(slot, U256::from(11))];
        let fixture = absent_account_post_state_fixture((1, U256::from(15)), &final_storage);
        let bundle = post_state_bundle(
            None,
            Some(account_info(1, 15)),
            AccountStatus::InMemoryChange,
            &[(slot, U256::ZERO, U256::from(11))],
        );
        let expected = fixture_target_header(&fixture).state_root;
        let db = WitnessDb::from_bundle(fixture).expect("verified account exclusion witness");

        assert_eq!(
            db.into_verified_post_state_root(&bundle)
                .expect("create account with first storage"),
            expected
        );
    }

    #[test]
    fn mixed_storage_insert_update_delete_uses_canonical_batches() {
        let slots = slots_with_distinct_root_nibbles(3);
        let (updated, deleted, inserted) = (slots[0], slots[1], slots[2]);
        let initial_storage = [(updated, U256::from(1)), (deleted, U256::from(2))];
        let final_storage = [(updated, U256::from(3)), (inserted, U256::from(4))];
        let fixture = post_state_fixture(
            (2, U256::from(20)),
            &initial_storage,
            Some((2, U256::from(20))),
            &final_storage,
            &[updated, deleted],
            false,
        );
        let bundle = post_state_bundle(
            Some(account_info(2, 20)),
            Some(account_info(2, 20)),
            AccountStatus::Changed,
            &[
                (updated, U256::from(1), U256::from(3)),
                (deleted, U256::from(2), U256::ZERO),
                (inserted, U256::ZERO, U256::from(4)),
            ],
        );
        let expected = fixture_target_header(&fixture).state_root;
        let db = WitnessDb::from_bundle(fixture).expect("verified mixed-storage witness");

        assert_eq!(
            db.into_verified_post_state_root(&bundle)
                .expect("mixed storage post-state root"),
            expected
        );
    }

    #[test]
    fn destroyed_and_recreated_account_wipes_old_storage() {
        let slots = slots_with_distinct_root_nibbles(3);
        let initial_storage = [(slots[0], U256::from(1)), (slots[1], U256::from(2))];
        let final_storage = [(slots[2], U256::from(7))];
        let fixture = post_state_fixture(
            (3, U256::from(30)),
            &initial_storage,
            Some((1, U256::from(5))),
            &final_storage,
            &[],
            false,
        );
        let bundle = post_state_bundle(
            Some(account_info(3, 30)),
            Some(account_info(1, 5)),
            AccountStatus::DestroyedChanged,
            &[(slots[2], U256::ZERO, U256::from(7))],
        );
        let expected = fixture_target_header(&fixture).state_root;
        let db = WitnessDb::from_bundle(fixture).expect("verified recreated-account witness");

        assert_eq!(
            db.into_verified_post_state_root(&bundle)
                .expect("wipe and recreate post-state root"),
            expected
        );
    }

    #[test]
    fn account_removal_without_revealed_sibling_fails_closed() {
        let fixture = post_state_fixture((4, U256::from(40)), &[], None, &[], &[], false);
        let bundle = post_state_bundle(
            Some(account_info(4, 40)),
            None,
            AccountStatus::Destroyed,
            &[],
        );
        let db = WitnessDb::from_bundle(fixture).expect("verified account witness");

        assert!(matches!(
            db.into_verified_post_state_root(&bundle),
            Err(WitnessImportError::PostStateAccountProofIncomplete { .. })
        ));
    }

    #[test]
    fn account_removal_with_revealed_sibling_is_verified() {
        let fixture = post_state_fixture((4, U256::from(40)), &[], None, &[], &[], true);
        let bundle = post_state_bundle(
            Some(account_info(4, 40)),
            None,
            AccountStatus::Destroyed,
            &[],
        );
        let expected = fixture_target_header(&fixture).state_root;
        let db = WitnessDb::from_bundle(fixture).expect("verified account and sibling witness");

        assert_eq!(
            db.into_verified_post_state_root(&bundle)
                .expect("account removal post-state root"),
            expected
        );
    }

    #[test]
    fn storage_removal_without_revealed_sibling_fails_closed() {
        let slots = slots_with_distinct_root_nibbles(2);
        let initial_storage = [(slots[0], U256::from(1)), (slots[1], U256::from(2))];
        let final_storage = [(slots[1], U256::from(2))];
        let fixture = post_state_fixture(
            (5, U256::from(50)),
            &initial_storage,
            Some((5, U256::from(50))),
            &final_storage,
            &[slots[0]],
            false,
        );
        let bundle = post_state_bundle(
            Some(account_info(5, 50)),
            Some(account_info(5, 50)),
            AccountStatus::Changed,
            &[(slots[0], U256::from(1), U256::ZERO)],
        );
        let db = WitnessDb::from_bundle(fixture).expect("verified partial storage witness");

        assert!(matches!(
            db.into_verified_post_state_root(&bundle),
            Err(WitnessImportError::PostStateStorageProofIncomplete { .. })
        ));
    }

    #[test]
    fn tampered_final_storage_value_fails_target_root_binding() {
        let slot = U256::from(6);
        let expected_storage = [(slot, U256::from(5))];
        let fixture = post_state_fixture(
            (6, U256::from(60)),
            &[],
            Some((6, U256::from(60))),
            &expected_storage,
            &[],
            false,
        );
        let tampered_bundle = post_state_bundle(
            Some(account_info(6, 60)),
            Some(account_info(6, 60)),
            AccountStatus::Changed,
            &[(slot, U256::ZERO, U256::from(99))],
        );
        let db = WitnessDb::from_bundle(fixture).expect("verified empty-storage witness");

        assert!(matches!(
            db.into_verified_post_state_root(&tampered_bundle),
            Err(WitnessImportError::PostStateRootMismatch { .. })
        ));
    }

    fn post_state_fixture(
        initial_account: (u64, U256),
        initial_storage: &[(U256, U256)],
        final_account: Option<(u64, U256)>,
        final_storage: &[(U256, U256)],
        revealed_storage_slots: &[U256],
        reveal_outside: bool,
    ) -> WitnessBundle {
        let (storage_root, storage_root_node, storage_leaf_nodes) =
            storage_proof_nodes(initial_storage);
        let initial_trie_account = TrieAccount {
            nonce: initial_account.0,
            balance: initial_account.1,
            storage_root,
            code_hash: KECCAK_EMPTY,
        };
        let account_path = Nibbles::unpack(keccak256(ACCOUNT));
        let account_node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            account_path.slice(1..),
            alloy_rlp::encode(initial_trie_account),
        )));
        let outside_path = Nibbles::unpack(keccak256(OUTSIDE));
        let outside_account = TrieAccount::default();
        let outside_node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            outside_path.slice(1..),
            alloy_rlp::encode(outside_account),
        )));
        let state_root_node = branch_node_many(&[
            (account_path.first().unwrap(), account_node.clone()),
            (outside_path.first().unwrap(), outside_node.clone()),
        ]);
        let state_root = keccak256(&state_root_node);
        assert_eq!(
            state_root,
            alloy_trie::root::state_root_unsorted([
                (keccak256(ACCOUNT), initial_trie_account),
                (keccak256(OUTSIDE), outside_account),
            ])
        );

        let final_storage_root = alloy_trie::root::storage_root_unhashed(
            final_storage
                .iter()
                .map(|(slot, value)| (B256::from(slot.to_be_bytes::<32>()), *value)),
        );
        let mut final_accounts = vec![(keccak256(OUTSIDE), outside_account)];
        if let Some((nonce, balance)) = final_account {
            let account = TrieAccount {
                nonce,
                balance,
                storage_root: final_storage_root,
                code_hash: KECCAK_EMPTY,
            };
            if nonce != 0 || !balance.is_zero() || final_storage_root != EMPTY_ROOT_HASH {
                final_accounts.push((keccak256(ACCOUNT), account));
            }
        }
        let final_state_root = alloy_trie::root::state_root_unsorted(final_accounts);

        let parent = Header {
            number: 99,
            state_root,
            ..Default::default()
        };
        let target = Header {
            number: 100,
            parent_hash: parent.hash_slow(),
            state_root: final_state_root,
            ..Default::default()
        };
        let mut state = vec![state_root_node.into(), account_node.into()];
        if reveal_outside {
            state.push(outside_node.into());
        }
        if !revealed_storage_slots.is_empty() {
            state.push(
                storage_root_node
                    .expect("revealed slots require nonempty storage")
                    .into(),
            );
            for slot in revealed_storage_slots {
                state.push(
                    storage_leaf_nodes
                        .get(slot)
                        .expect("revealed slot is in initial storage")
                        .clone()
                        .into(),
                );
            }
        }

        WitnessBundle {
            target_header: alloy_rlp::encode(&target).into(),
            target_block_hash: target.hash_slow(),
            target_block: None,
            witness: ExecutionWitness {
                state,
                codes: Vec::new(),
                keys: Vec::new(),
                headers: vec![alloy_rlp::encode(parent).into()],
            },
            access_manifest: AccessManifest::default(),
        }
    }

    fn absent_account_post_state_fixture(
        final_account: (u64, U256),
        final_storage: &[(U256, U256)],
    ) -> WitnessBundle {
        let outside_account = TrieAccount::default();
        let outside_path = Nibbles::unpack(keccak256(OUTSIDE));
        let outside_node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
            outside_path,
            alloy_rlp::encode(outside_account),
        )));
        let state_root = keccak256(&outside_node);
        assert_eq!(
            state_root,
            alloy_trie::root::state_root_unsorted([(keccak256(OUTSIDE), outside_account)])
        );

        let storage_root = alloy_trie::root::storage_root_unhashed(
            final_storage
                .iter()
                .map(|(slot, value)| (B256::from(slot.to_be_bytes::<32>()), *value)),
        );
        let final_account = TrieAccount {
            nonce: final_account.0,
            balance: final_account.1,
            storage_root,
            code_hash: KECCAK_EMPTY,
        };
        let final_state_root = alloy_trie::root::state_root_unsorted([
            (keccak256(ACCOUNT), final_account),
            (keccak256(OUTSIDE), outside_account),
        ]);
        let parent = Header {
            number: 99,
            state_root,
            ..Default::default()
        };
        let target = Header {
            number: 100,
            parent_hash: parent.hash_slow(),
            state_root: final_state_root,
            ..Default::default()
        };

        WitnessBundle {
            target_header: alloy_rlp::encode(&target).into(),
            target_block_hash: target.hash_slow(),
            target_block: None,
            witness: ExecutionWitness {
                state: vec![outside_node.into()],
                codes: Vec::new(),
                keys: Vec::new(),
                headers: vec![alloy_rlp::encode(parent).into()],
            },
            access_manifest: AccessManifest::default(),
        }
    }

    fn storage_proof_nodes(
        storage: &[(U256, U256)],
    ) -> (B256, Option<Vec<u8>>, BTreeMap<U256, Vec<u8>>) {
        if storage.is_empty() {
            return (EMPTY_ROOT_HASH, None, BTreeMap::new());
        }

        let mut leaf_nodes = BTreeMap::new();
        let root_node = if storage.len() == 1 {
            let (slot, value) = storage[0];
            let path = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes::<32>())));
            let node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
                path,
                alloy_rlp::encode_fixed_size(&value).to_vec(),
            )));
            leaf_nodes.insert(slot, node.clone());
            node
        } else {
            let mut children = Vec::with_capacity(storage.len());
            for &(slot, value) in storage {
                let path = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes::<32>())));
                let node = alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
                    path.slice(1..),
                    alloy_rlp::encode_fixed_size(&value).to_vec(),
                )));
                children.push((path.first().unwrap(), node.clone()));
                leaf_nodes.insert(slot, node);
            }
            branch_node_many(&children)
        };
        let root = keccak256(&root_node);
        assert_eq!(
            root,
            alloy_trie::root::storage_root_unhashed(
                storage
                    .iter()
                    .map(|(slot, value)| (B256::from(slot.to_be_bytes::<32>()), *value))
            )
        );
        (root, Some(root_node), leaf_nodes)
    }

    fn branch_node_many(children: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut children = children.to_vec();
        children.sort_unstable_by_key(|child| child.0);
        assert!(
            children.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "fixture paths must branch at the root"
        );
        let stack = children
            .iter()
            .map(|(_, node)| RlpNode::from_rlp(node))
            .collect();
        let state_mask = TrieMask::new(
            children
                .iter()
                .fold(0u16, |mask, (nibble, _)| mask | (1u16 << nibble)),
        );
        alloy_rlp::encode(TrieNodeV2::Branch(BranchNodeV2::new(
            Nibbles::default(),
            stack,
            state_mask,
            None,
        )))
    }

    fn slots_with_distinct_root_nibbles(count: usize) -> Vec<U256> {
        let mut slots = Vec::with_capacity(count);
        let mut seen = BTreeSet::new();
        for value in 0u64.. {
            let slot = U256::from(value);
            let path = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes::<32>())));
            if seen.insert(path.first().unwrap()) {
                slots.push(slot);
                if slots.len() == count {
                    return slots;
                }
            }
        }
        unreachable!("a hash has at least sixteen root nibbles")
    }

    fn post_state_bundle(
        original_info: Option<AccountInfo>,
        present_info: Option<AccountInfo>,
        status: AccountStatus,
        storage: &[(U256, U256, U256)],
    ) -> BundleState {
        let storage = storage
            .iter()
            .map(|(slot, original, present)| (*slot, StorageSlot::new_changed(*original, *present)))
            .collect();
        let account = BundleAccount::new(original_info, present_info, storage, status);
        BundleState {
            state: [(ACCOUNT, account)].into_iter().collect(),
            ..Default::default()
        }
    }

    fn account_info(nonce: u64, balance: u64) -> AccountInfo {
        AccountInfo {
            balance: U256::from(balance),
            nonce,
            code_hash: KECCAK_EMPTY,
            code: None,
            account_id: None,
        }
    }

    #[test]
    fn no_manifest_lazily_reads_inclusion_and_exclusion_proofs() {
        let fixture = fixture();
        let expected_root = fixture.witness.headers[0].clone();
        let mut db = WitnessDb::from_bundle(fixture).expect("verified witness");

        assert_eq!(db.parent_header().number, 99);
        assert_eq!(db.target_header().number, 100);
        assert_eq!(db.verified_root().unwrap(), db.pre_state_root());
        assert!(!expected_root.is_empty());

        let account = db.basic(ACCOUNT).unwrap().expect("proven account");
        assert_eq!(account.nonce, 7);
        assert_eq!(account.balance, U256::from(1_000_000));
        assert_eq!(
            db.code_by_hash(account.code_hash).unwrap().original_bytes(),
            Bytes::from_static(&[0x60, 0x00, 0x56])
        );
        assert_eq!(db.storage(ACCOUNT, U256::ZERO).unwrap(), U256::from(42));
        assert_eq!(db.basic(ABSENT).unwrap(), None);
        assert_eq!(db.storage(ABSENT, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(db.block_hash(99).unwrap(), db.parent_header().hash_slow());

        assert_eq!(
            db.basic(OUTSIDE),
            Err(WitnessImportError::IncompleteAccountProof(OUTSIDE))
        );
        assert_eq!(
            db.storage(ACCOUNT, U256::from(1)),
            Err(WitnessImportError::IncompleteStorageProof {
                address: ACCOUNT,
                slot: B256::from(U256::from(1).to_be_bytes::<32>()),
            })
        );
        assert_eq!(
            db.code_by_hash(B256::repeat_byte(0x44)),
            Err(WitnessImportError::StrictDatabase(
                StrictDbError::MissingCode(B256::repeat_byte(0x44))
            ))
        );
        assert_eq!(
            db.block_hash(98),
            Err(WitnessImportError::StrictDatabase(
                StrictDbError::MissingBlockHash(98)
            ))
        );
    }

    #[test]
    fn storage_by_account_id_resolves_proof_but_rejects_unproven_id() {
        let mut db = WitnessDb::from_bundle(fixture()).expect("verified witness");
        let account_id = AccountId::new(7).unwrap();
        assert_eq!(
            db.storage_by_account_id(ACCOUNT, account_id, U256::ZERO),
            Err(WitnessImportError::StrictDatabase(
                StrictDbError::InvalidAccountId {
                    address: ACCOUNT,
                    account_id,
                }
            ))
        );
        assert_eq!(
            db.strict_db_mut().storage(ACCOUNT, U256::ZERO).unwrap(),
            U256::from(42)
        );
    }

    #[test]
    fn json_without_manifest_defaults_to_lazy_resolution() {
        let fixture = fixture();
        let mut json = serde_json::to_value(&fixture).unwrap();
        assert!(
            json.get("targetBlock").is_none(),
            "legacy fixture must omit the optional raw block"
        );
        json.as_object_mut().unwrap().remove("accessManifest");
        json.as_object_mut().unwrap().remove("targetBlock");
        let json = serde_json::to_vec(&json).unwrap();
        let db = WitnessDb::from_json(&json).expect("round-tripped witness");
        assert_eq!(db.parent_header().number, 99);
        assert_eq!(db.access_manifest(), &AccessManifest::default());
        assert_eq!(db.target_block(), None);
        assert_eq!(db.target_block_transaction_count(), None);
    }

    #[test]
    fn optional_manifest_eagerly_preloads_strict_database() {
        let mut fixture = fixture();
        fixture.access_manifest = AccessManifest {
            accounts: vec![ACCOUNT, ABSENT],
            storage: vec![
                StorageAccess {
                    address: ACCOUNT,
                    slot: B256::ZERO,
                },
                StorageAccess {
                    address: ABSENT,
                    slot: B256::ZERO,
                },
            ],
        };
        let mut db = WitnessDb::from_bundle(fixture).expect("verified witness");
        assert!(db.strict_db_mut().basic(ACCOUNT).unwrap().is_some());
        assert_eq!(
            db.strict_db_mut().storage(ACCOUNT, U256::ZERO).unwrap(),
            U256::from(42)
        );
        assert_eq!(db.strict_db_mut().basic(ABSENT).unwrap(), None);
    }

    #[test]
    fn removed_storage_node_fails_instead_of_returning_zero() {
        let mut fixture = fixture();
        fixture.witness.state.pop();
        let mut db = WitnessDb::from_bundle(fixture).expect("account proof remains valid");
        assert!(matches!(
            db.storage(ACCOUNT, U256::ZERO),
            Err(WitnessImportError::IncompleteStorageProof { address, slot })
                if address == ACCOUNT && slot == B256::ZERO
        ));
    }

    #[test]
    fn tampered_root_node_is_rejected() {
        let mut fixture = fixture();
        let mut tampered = fixture.witness.state[0].to_vec();
        tampered[0] ^= 1;
        fixture.witness.state[0] = tampered.into();
        assert!(matches!(
            WitnessDb::from_bundle(fixture),
            Err(WitnessImportError::MissingStateRoot(_))
        ));
    }

    #[test]
    fn malformed_auxiliary_key_is_rejected() {
        let mut fixture = fixture();
        fixture.witness.keys.push(Bytes::from_static(&[0u8; 31]));
        assert!(matches!(
            WitnessDb::from_bundle(fixture),
            Err(WitnessImportError::InvalidKeyLength {
                index: 2,
                length: 31
            })
        ));
    }

    #[test]
    fn mismatched_target_parent_is_rejected() {
        let mut fixture = fixture();
        let mut target = fixture_target_header(&fixture);
        target.parent_hash = B256::ZERO;
        fixture.target_header = alloy_rlp::encode(&target).into();
        fixture.target_block_hash = target.hash_slow();
        assert!(matches!(
            WitnessDb::from_bundle(fixture),
            Err(WitnessImportError::TargetParentHashMismatch { .. })
        ));
    }

    #[test]
    fn mismatched_target_hash_is_rejected() {
        let mut fixture = fixture();
        fixture.target_block_hash = B256::ZERO;
        assert!(matches!(
            WitnessDb::from_bundle(fixture),
            Err(WitnessImportError::TargetBlockHashMismatch { .. })
        ));
    }

    #[test]
    fn raw_target_block_is_bound_and_retained() {
        let fixture = raw_bound_fixture();
        let raw = fixture.target_block.clone().unwrap();
        let db = WitnessDb::from_bundle(fixture).expect("raw target block binding");
        assert_eq!(db.target_block(), Some(&raw));
        assert_eq!(db.target_block_transaction_count(), Some(0));
    }

    #[test]
    fn invalid_raw_target_block_is_rejected() {
        let mut fixture = raw_bound_fixture();
        fixture.target_block = Some(Bytes::from_static(&[0xff]));
        assert!(matches!(
            WitnessDb::from_bundle(fixture),
            Err(WitnessImportError::InvalidTargetBlock(_))
        ));
    }

    #[test]
    fn trailing_raw_target_block_bytes_are_rejected() {
        let mut fixture = raw_bound_fixture();
        let mut raw = fixture.target_block.take().unwrap().to_vec();
        raw.push(0);
        fixture.target_block = Some(raw.into());
        assert_eq!(
            WitnessDb::from_bundle(fixture).unwrap_err(),
            WitnessImportError::TargetBlockTrailingBytes
        );
    }

    #[test]
    fn mismatched_raw_target_header_is_rejected() {
        let mut fixture = raw_bound_fixture();
        let mut block = fixture_target_block(&fixture);
        block.header.timestamp = block.header.timestamp.saturating_add(1);
        fixture.target_block = Some(alloy_rlp::encode(block).into());
        assert_eq!(
            WitnessDb::from_bundle(fixture).unwrap_err(),
            WitnessImportError::TargetBlockHeaderMismatch
        );
    }

    #[test]
    fn mismatched_raw_target_hash_is_rejected() {
        let fixture = raw_bound_fixture();
        let raw = fixture.target_block.as_ref().unwrap();
        let target_header = fixture_target_header(&fixture);
        assert!(matches!(
            validate_target_block(raw, &target_header, B256::ZERO),
            Err(WitnessImportError::TargetRawBlockHashMismatch { .. })
        ));
    }

    #[test]
    fn mismatched_raw_transactions_root_is_rejected() {
        let mut fixture = raw_bound_fixture();
        let mut block = fixture_target_block(&fixture);
        block.header.transactions_root = B256::repeat_byte(0x11);
        bind_fixture_to_raw_block(&mut fixture, block);
        assert!(matches!(
            WitnessDb::from_bundle(fixture),
            Err(WitnessImportError::TargetBlockTransactionsRootMismatch { .. })
        ));
    }

    #[test]
    fn mismatched_raw_ommers_hash_is_rejected() {
        let mut fixture = raw_bound_fixture();
        let mut block = fixture_target_block(&fixture);
        block.header.ommers_hash = B256::repeat_byte(0x22);
        bind_fixture_to_raw_block(&mut fixture, block);
        assert!(matches!(
            WitnessDb::from_bundle(fixture),
            Err(WitnessImportError::TargetBlockOmmersHashMismatch { .. })
        ));
    }

    #[test]
    fn mismatched_raw_withdrawals_root_is_rejected() {
        let mut fixture = raw_bound_fixture();
        let mut block = fixture_target_block(&fixture);
        block.header.withdrawals_root = Some(B256::repeat_byte(0x33));
        bind_fixture_to_raw_block(&mut fixture, block);
        let error = WitnessDb::from_bundle(fixture).unwrap_err();
        assert!(
            matches!(
                error,
                WitnessImportError::TargetBlockWithdrawalsRootMismatch { .. }
            ),
            "{error:?}"
        );
    }

    fn raw_bound_fixture() -> WitnessBundle {
        let mut fixture = fixture();
        let mut target = fixture_target_header(&fixture);
        target.base_fee_per_gas = Some(1);
        let mut block = TargetBlock::from_transactions(target, std::iter::empty());
        block.body.withdrawals = Some(Default::default());
        block.header.withdrawals_root = block.body.calculate_withdrawals_root();
        bind_fixture_to_raw_block(&mut fixture, block);
        fixture
    }

    fn fixture_target_block(fixture: &WitnessBundle) -> TargetBlock {
        let mut input = fixture
            .target_block
            .as_ref()
            .expect("fixture raw target block")
            .as_ref();
        let block = TargetBlock::decode(&mut input).expect("fixture target block");
        assert!(input.is_empty());
        block
    }

    fn bind_fixture_to_raw_block(fixture: &mut WitnessBundle, block: TargetBlock) {
        fixture.target_header = alloy_rlp::encode(&block.header).into();
        fixture.target_block_hash = block.header.hash_slow();
        fixture.target_block = Some(alloy_rlp::encode(block).into());
    }

    fn fixture_target_header(fixture: &WitnessBundle) -> Header {
        let mut input = fixture.target_header.as_ref();
        Header::decode(&mut input).expect("fixture target header")
    }
}
