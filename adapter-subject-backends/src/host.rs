//! EVMC host callbacks with sticky fail-closed fault propagation.

use crate::ffi::{
    EvmcAddress, EvmcBytes32, EvmcHostInterface, EvmcMessage, EvmcResult, EvmcTxContext,
    EvmcTxInitcode, EvmcVm, ExecuteFn, HostContextOpaque, EVMC_ARGUMENT_OUT_OF_RANGE,
    EVMC_BAD_JUMP_DESTINATION, EVMC_CALL, EVMC_CALLCODE, EVMC_CALL_DEPTH_EXCEEDED,
    EVMC_CONTRACT_VALIDATION_FAILURE, EVMC_CREATE, EVMC_CREATE2, EVMC_DELEGATECALL, EVMC_DELEGATED,
    EVMC_FAILURE, EVMC_INSUFFICIENT_BALANCE, EVMC_INVALID_INSTRUCTION, EVMC_INVALID_MEMORY_ACCESS,
    EVMC_OUT_OF_GAS, EVMC_PRECOMPILE_FAILURE, EVMC_REVERT, EVMC_STACK_OVERFLOW,
    EVMC_STACK_UNDERFLOW, EVMC_STATIC, EVMC_STATIC_MODE_VIOLATION, EVMC_SUCCESS,
    EVMC_UNDEFINED_INSTRUCTION,
};
use alloy_primitives::keccak256;
use core::ffi::c_int;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    panic::{catch_unwind, AssertUnwindSafe},
    ptr::{self, NonNull},
    rc::Rc,
    slice,
};
use thiserror::Error;

pub const EVMC_STORAGE_ASSIGNED: i32 = 0;
pub const EVMC_STORAGE_ADDED: i32 = 1;
pub const EVMC_STORAGE_DELETED: i32 = 2;
pub const EVMC_STORAGE_MODIFIED: i32 = 3;
pub const EVMC_STORAGE_DELETED_ADDED: i32 = 4;
pub const EVMC_STORAGE_MODIFIED_DELETED: i32 = 5;
pub const EVMC_STORAGE_DELETED_RESTORED: i32 = 6;
pub const EVMC_STORAGE_ADDED_DELETED: i32 = 7;
pub const EVMC_STORAGE_MODIFIED_RESTORED: i32 = 8;

pub const EVMC_ACCESS_COLD: i32 = 0;
pub const EVMC_ACCESS_WARM: i32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(pub [u8; 20]);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Word(pub [u8; 32]);

impl Word {
    pub const ZERO: Self = Self([0; 32]);

    pub fn from_u64(value: u64) -> Self {
        let mut bytes = [0; 32];
        bytes[24..].copy_from_slice(&value.to_be_bytes());
        Self(bytes)
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }
}

impl From<EvmcAddress> for Address {
    fn from(value: EvmcAddress) -> Self {
        Self(value.bytes)
    }
}

impl From<Address> for EvmcAddress {
    fn from(value: Address) -> Self {
        Self { bytes: value.0 }
    }
}

impl From<EvmcBytes32> for Word {
    fn from(value: EvmcBytes32) -> Self {
        Self(value.bytes)
    }
}

impl From<Word> for EvmcBytes32 {
    fn from(value: Word) -> Self {
        Self { bytes: value.0 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub address: Address,
    pub data: Vec<u8>,
    pub topics: Vec<Word>,
}

/// Plain EVMC call data passed across the backend boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NestedCallRequest {
    pub kind: i32,
    pub flags: u32,
    pub depth: i32,
    pub gas: u64,
    pub recipient: Address,
    pub sender: Address,
    pub input: Vec<u8>,
    pub value: Word,
    pub create2_salt: Word,
    pub code_address: Address,
}

/// Backend-normalized result returned to the calling DTVM frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NestedCallResult {
    pub status_code: i32,
    pub gas_left: u64,
    pub gas_refund: i64,
    pub output: Vec<u8>,
    pub create_address: Option<Address>,
}

/// Result of asking the Reth shell to initialize one child call or creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NestedCallPreparation {
    /// Reth completed the call without executing bytecode (for example an
    /// empty account, a precompile, depth failure, or insufficient funds).
    Immediate(NestedCallResult),
    /// The initialized Reth child frame owns the checkpoint while this code is
    /// synchronously executed by the same DTVM instance. Creation rewrites the
    /// recipient to Reth's derived address and clears calldata while retaining
    /// the original initcode as `code`.
    Execute {
        code: Vec<u8>,
        recipient: Address,
        input: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessEvent {
    AccountExists(Address),
    StorageRead(Address, Word),
    StorageWrite(Address, Word, Word),
    Balance(Address),
    CodeSize(Address),
    CodeHash(Address),
    CodeCopy(Address, usize, usize),
    Selfdestruct(Address, Address),
    NestedCall(i32),
    TxContext,
    BlockHash(i64),
    Log(Address, usize, usize),
    AccountWarm(Address),
    StorageWarm(Address, Word),
    TransientRead(Address, Word),
    TransientWrite(Address, Word, Word),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HostFault {
    #[error("account is outside the proven witness: {0:?}")]
    MissingAccount(Address),
    #[error("storage is outside the proven witness: {0:?}[{1:?}]")]
    MissingStorage(Address, Word),
    #[error("block hash is outside the proven witness: {0}")]
    MissingBlockHash(i64),
    #[error("nested call kind is unsupported (kind={kind})")]
    NestedCallUnsupported { kind: i32 },
    #[error("nested CALL flags are unsupported (flags=0x{flags:x})")]
    NestedCallFlagsUnsupported { flags: u32 },
    #[error("nested CALL code address differs from its recipient")]
    NestedCallCodeAddressMismatch,
    #[error("nested CALL has an invalid depth: {0}")]
    NestedCallDepthInvalid(i32),
    #[error("nested CALL gas is outside the EVMC int64 range: {0}")]
    NestedCallGasInvalid(i64),
    #[error("SELFDESTRUCT is unsupported by this host backend")]
    SelfdestructUnsupported,
    #[error("top-level account must exist in this VM-level slice: {0:?}")]
    TopLevelAccountAbsent(Address),
    #[error("top-level account was not prewarmed by the transaction shell: {0:?}")]
    TopLevelAccountCold(Address),
    #[error("supplied execute code differs from authoritative witness code at {0:?}")]
    CodeMismatch(Address),
    #[error("authoritative code hash does not match authoritative code bytes at {0:?}")]
    CodeHashMismatch(Address),
    #[error("system call execution is unsupported by this adapter entry point")]
    SystemCallUnsupported,
    #[error("inspector execution is unsupported by this adapter entry point")]
    InspectorUnsupported,
    #[error("invalid pointer or pointer/length pair from EVMC: {what}")]
    InvalidPointer { what: &'static str },
    #[error("EVMC callback panicked: {callback}")]
    CallbackPanicked { callback: &'static str },
    #[error("invalid log topic count: {0}")]
    InvalidTopicCount(usize),
    #[error("backend fault: {0}")]
    Backend(String),
}

/// State interface used by the C callbacks.
///
/// Implementations must make missing witness data an error. Returning a zero
/// value is permitted only for data explicitly proven absent or zero.
pub trait HostBackend {
    fn checkpoint(&mut self) -> Result<(), HostFault>;
    /// Commits the current checkpoint.
    ///
    /// On error the checkpoint must remain active so the adapter can make a
    /// best-effort `revert_checkpoint` call.
    fn commit_checkpoint(&mut self) -> Result<(), HostFault>;
    fn revert_checkpoint(&mut self) -> Result<(), HostFault>;

    fn account_exists(&mut self, address: Address) -> Result<bool, HostFault>;
    fn get_storage(&mut self, address: Address, key: Word) -> Result<Word, HostFault>;
    fn set_storage(&mut self, address: Address, key: Word, value: Word) -> Result<i32, HostFault>;
    fn get_balance(&mut self, address: Address) -> Result<Word, HostFault>;
    fn get_code(&mut self, address: Address) -> Result<Vec<u8>, HostFault>;
    fn get_code_hash(&mut self, address: Address) -> Result<Word, HostFault>;
    fn selfdestruct(&mut self, address: Address, beneficiary: Address) -> Result<bool, HostFault>;
    fn block_hash(&mut self, number: i64) -> Result<Word, HostFault>;
    fn emit_log(&mut self, log: LogEntry) -> Result<(), HostFault>;
    fn access_account(&mut self, address: Address) -> Result<bool, HostFault>;
    fn is_account_warm(&mut self, address: Address) -> Result<bool, HostFault>;
    fn access_storage(&mut self, address: Address, key: Word) -> Result<bool, HostFault>;
    fn get_transient_storage(&mut self, address: Address, key: Word) -> Result<Word, HostFault>;
    fn set_transient_storage(
        &mut self,
        address: Address,
        key: Word,
        value: Word,
    ) -> Result<(), HostFault>;

    /// Initializes one child call or creation in the owning client.
    ///
    /// The default keeps non-Reth backends fail-closed. A successful
    /// [`NestedCallPreparation::Execute`] must leave exactly one child
    /// checkpoint active until the matching [`HostBackend::finish_call`].
    fn begin_call(
        &mut self,
        request: &NestedCallRequest,
    ) -> Result<NestedCallPreparation, HostFault> {
        Err(HostFault::NestedCallUnsupported { kind: request.kind })
    }

    /// Resolves the child initialized by [`HostBackend::begin_call`].
    fn finish_call(&mut self, _result: NestedCallResult) -> Result<NestedCallResult, HostFault> {
        Err(HostFault::Backend(
            "nested call finish requested without backend support".into(),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StorageSlot {
    original: Word,
    present: Word,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    balance: Word,
    code: Vec<u8>,
    code_hash: Word,
    storage: BTreeMap<Word, StorageSlot>,
}

impl Account {
    pub fn new(balance: Word, code: Vec<u8>) -> Self {
        let code_hash = Word(keccak256(&code).0);
        Self {
            balance,
            code,
            code_hash,
            storage: BTreeMap::new(),
        }
    }

    pub fn set_original_storage(&mut self, key: Word, value: Word) {
        self.storage.insert(
            key,
            StorageSlot {
                original: value,
                present: value,
            },
        );
    }

    pub fn present_storage(&self, key: Word) -> Option<Word> {
        self.storage.get(&key).map(|slot| slot.present)
    }

    pub fn code_hash(&self) -> Word {
        self.code_hash
    }
}

#[derive(Clone, Debug)]
struct WitnessSnapshot {
    accounts: BTreeMap<Address, Account>,
    warm_accounts: BTreeSet<Address>,
    warm_storage: BTreeSet<(Address, Word)>,
    transient_storage: BTreeMap<(Address, Word), Word>,
    logs: Vec<LogEntry>,
}

/// Explicit witness-backed in-memory backend used by the first executable
/// adapter and differential tests.
#[derive(Clone, Debug, Default)]
pub struct WitnessBackend {
    covered_accounts: BTreeSet<Address>,
    covered_storage: BTreeSet<(Address, Word)>,
    accounts: BTreeMap<Address, Account>,
    block_hashes: BTreeMap<i64, Word>,
    warm_accounts: BTreeSet<Address>,
    warm_storage: BTreeSet<(Address, Word)>,
    transient_storage: BTreeMap<(Address, Word), Word>,
    logs: Vec<LogEntry>,
    checkpoints: Vec<WitnessSnapshot>,
}

impl WitnessBackend {
    pub fn cover_absent_account(&mut self, address: Address) {
        self.covered_accounts.insert(address);
        self.accounts.remove(&address);
        self.covered_storage
            .retain(|(covered, _)| *covered != address);
    }

    pub fn insert_account(&mut self, address: Address, mut account: Account) {
        account.code_hash = Word(keccak256(&account.code).0);
        self.covered_accounts.insert(address);
        self.covered_storage
            .retain(|(covered, _)| *covered != address);
        self.accounts.insert(address, account);
    }

    pub fn cover_storage(
        &mut self,
        address: Address,
        key: Word,
        value: Word,
    ) -> Result<(), HostFault> {
        self.require_account(address)?;
        let account = self
            .accounts
            .get_mut(&address)
            .ok_or(HostFault::TopLevelAccountAbsent(address))?;
        self.covered_storage.insert((address, key));
        account.set_original_storage(key, value);
        Ok(())
    }

    pub fn insert_block_hash(&mut self, number: i64, hash: Word) {
        self.block_hashes.insert(number, hash);
    }

    pub fn prewarm_account(&mut self, address: Address) -> Result<(), HostFault> {
        self.require_account(address)?;
        self.warm_accounts.insert(address);
        Ok(())
    }

    pub fn prewarm_storage(&mut self, address: Address, key: Word) -> Result<(), HostFault> {
        self.require_storage(address, key)?;
        self.warm_storage.insert((address, key));
        Ok(())
    }

    pub fn logs(&self) -> &[LogEntry] {
        &self.logs
    }

    pub fn account(&self, address: Address) -> Option<&Account> {
        self.accounts.get(&address)
    }

    fn require_account(&self, address: Address) -> Result<(), HostFault> {
        if self.covered_accounts.contains(&address) {
            Ok(())
        } else {
            Err(HostFault::MissingAccount(address))
        }
    }

    fn require_storage(&self, address: Address, key: Word) -> Result<(), HostFault> {
        self.require_account(address)?;
        if self.covered_storage.contains(&(address, key)) {
            Ok(())
        } else {
            Err(HostFault::MissingStorage(address, key))
        }
    }
}

impl HostBackend for WitnessBackend {
    fn checkpoint(&mut self) -> Result<(), HostFault> {
        self.checkpoints.push(WitnessSnapshot {
            accounts: self.accounts.clone(),
            warm_accounts: self.warm_accounts.clone(),
            warm_storage: self.warm_storage.clone(),
            transient_storage: self.transient_storage.clone(),
            logs: self.logs.clone(),
        });
        Ok(())
    }

    fn commit_checkpoint(&mut self) -> Result<(), HostFault> {
        self.checkpoints.pop().ok_or_else(|| {
            HostFault::Backend("commit requested without an active checkpoint".into())
        })?;
        Ok(())
    }

    fn revert_checkpoint(&mut self) -> Result<(), HostFault> {
        let snapshot = self.checkpoints.pop().ok_or_else(|| {
            HostFault::Backend("revert requested without an active checkpoint".into())
        })?;
        self.accounts = snapshot.accounts;
        self.warm_accounts = snapshot.warm_accounts;
        self.warm_storage = snapshot.warm_storage;
        self.transient_storage = snapshot.transient_storage;
        self.logs = snapshot.logs;
        Ok(())
    }

    fn account_exists(&mut self, address: Address) -> Result<bool, HostFault> {
        self.require_account(address)?;
        Ok(self.accounts.contains_key(&address))
    }

    fn get_storage(&mut self, address: Address, key: Word) -> Result<Word, HostFault> {
        self.require_storage(address, key)?;
        Ok(self
            .accounts
            .get(&address)
            .and_then(|account| account.storage.get(&key))
            .map(|slot| slot.present)
            .unwrap_or(Word::ZERO))
    }

    fn set_storage(&mut self, address: Address, key: Word, value: Word) -> Result<i32, HostFault> {
        self.require_storage(address, key)?;
        let account = self
            .accounts
            .get_mut(&address)
            .ok_or(HostFault::MissingAccount(address))?;
        let slot = account.storage.entry(key).or_insert(StorageSlot {
            original: Word::ZERO,
            present: Word::ZERO,
        });
        let status = classify_storage_status(slot.original, slot.present, value);
        slot.present = value;
        Ok(status)
    }

    fn get_balance(&mut self, address: Address) -> Result<Word, HostFault> {
        self.require_account(address)?;
        Ok(self
            .accounts
            .get(&address)
            .map(|account| account.balance)
            .unwrap_or(Word::ZERO))
    }

    fn get_code(&mut self, address: Address) -> Result<Vec<u8>, HostFault> {
        self.require_account(address)?;
        Ok(self
            .accounts
            .get(&address)
            .map(|account| account.code.clone())
            .unwrap_or_default())
    }

    fn get_code_hash(&mut self, address: Address) -> Result<Word, HostFault> {
        self.require_account(address)?;
        Ok(self
            .accounts
            .get(&address)
            .map(|account| account.code_hash)
            .unwrap_or(Word::ZERO))
    }

    fn selfdestruct(
        &mut self,
        _address: Address,
        _beneficiary: Address,
    ) -> Result<bool, HostFault> {
        Err(HostFault::SelfdestructUnsupported)
    }

    fn block_hash(&mut self, number: i64) -> Result<Word, HostFault> {
        self.block_hashes
            .get(&number)
            .copied()
            .ok_or(HostFault::MissingBlockHash(number))
    }

    fn emit_log(&mut self, log: LogEntry) -> Result<(), HostFault> {
        self.require_account(log.address)?;
        self.logs.push(log);
        Ok(())
    }

    fn access_account(&mut self, address: Address) -> Result<bool, HostFault> {
        self.require_account(address)?;
        Ok(!self.warm_accounts.insert(address))
    }

    fn is_account_warm(&mut self, address: Address) -> Result<bool, HostFault> {
        self.require_account(address)?;
        Ok(self.warm_accounts.contains(&address))
    }

    fn access_storage(&mut self, address: Address, key: Word) -> Result<bool, HostFault> {
        self.require_storage(address, key)?;
        Ok(!self.warm_storage.insert((address, key)))
    }

    fn get_transient_storage(&mut self, address: Address, key: Word) -> Result<Word, HostFault> {
        self.require_account(address)?;
        Ok(self
            .transient_storage
            .get(&(address, key))
            .copied()
            .unwrap_or(Word::ZERO))
    }

    fn set_transient_storage(
        &mut self,
        address: Address,
        key: Word,
        value: Word,
    ) -> Result<(), HostFault> {
        self.require_account(address)?;
        self.transient_storage.insert((address, key), value);
        Ok(())
    }
}

/// Converts the `(original, current, new)` triple to the exact EVMC status.
pub fn classify_storage_status(original: Word, present: Word, new: Word) -> i32 {
    if new == present {
        return EVMC_STORAGE_ASSIGNED;
    }
    if original == present {
        if original.is_zero() {
            return EVMC_STORAGE_ADDED;
        }
        if new.is_zero() {
            return EVMC_STORAGE_DELETED;
        }
        return EVMC_STORAGE_MODIFIED;
    }
    if present.is_zero() {
        if new == original {
            return EVMC_STORAGE_DELETED_RESTORED;
        }
        return EVMC_STORAGE_DELETED_ADDED;
    }
    if new.is_zero() {
        if original.is_zero() {
            return EVMC_STORAGE_ADDED_DELETED;
        }
        return EVMC_STORAGE_MODIFIED_DELETED;
    }
    if new == original {
        return EVMC_STORAGE_MODIFIED_RESTORED;
    }
    EVMC_STORAGE_ASSIGNED
}

/// Owns pointer-backed transaction context fields for the duration of execute.
#[derive(Debug)]
pub struct TxContextOwned {
    raw: EvmcTxContext,
    blob_hashes: Vec<EvmcBytes32>,
    initcodes: Vec<EvmcTxInitcode>,
    initcode_bytes: Vec<Vec<u8>>,
}

impl Default for TxContextOwned {
    fn default() -> Self {
        Self::new(EvmcTxContext::default(), Vec::new(), Vec::new())
    }
}

impl TxContextOwned {
    pub fn new(
        mut raw: EvmcTxContext,
        blob_hashes: Vec<EvmcBytes32>,
        initcode_bytes: Vec<(Word, Vec<u8>)>,
    ) -> Self {
        let bytes = initcode_bytes
            .iter()
            .map(|(_, code)| code.clone())
            .collect::<Vec<_>>();
        let initcodes = initcode_bytes
            .iter()
            .zip(bytes.iter())
            .map(|((hash, _), code)| EvmcTxInitcode {
                hash: (*hash).into(),
                code: code.as_ptr(),
                code_size: code.len(),
            })
            .collect::<Vec<_>>();
        raw.blob_hashes = null_if_empty(&blob_hashes);
        raw.blob_hashes_count = blob_hashes.len();
        raw.initcodes = null_if_empty(&initcodes);
        raw.initcodes_count = initcodes.len();
        Self {
            raw,
            blob_hashes,
            initcodes,
            initcode_bytes: bytes,
        }
    }

    pub fn raw(&self) -> EvmcTxContext {
        self.raw
    }

    pub fn backing_lengths(&self) -> (usize, usize, usize) {
        (
            self.blob_hashes.len(),
            self.initcodes.len(),
            self.initcode_bytes.len(),
        )
    }
}

fn null_if_empty<T>(values: &[T]) -> *const T {
    if values.is_empty() {
        ptr::null()
    } else {
        values.as_ptr()
    }
}

struct HostState<'a> {
    backend: &'a mut dyn HostBackend,
    audit: Vec<AccessEvent>,
}

impl HostState<'_> {
    fn checked_code_and_hash(&mut self, address: Address) -> Result<(Vec<u8>, Word), HostFault> {
        let exists = self.backend.account_exists(address)?;
        let code = self.backend.get_code(address)?;
        let code_hash = self.backend.get_code_hash(address)?;
        let valid = if exists {
            code_hash == Word(keccak256(&code).0)
        } else {
            code.is_empty() && code_hash == Word::ZERO
        };
        if !valid {
            return Err(HostFault::CodeHashMismatch(address));
        }
        Ok((code, code_hash))
    }
}

struct HostSession<'a> {
    state: RefCell<HostState<'a>>,
    fault: RefCell<Option<HostFault>>,
    tx_context: TxContextOwned,
    reentrant_vm: Cell<Option<ReentrantVm>>,
}

impl HostSession<'_> {
    fn latch(&self, fault: HostFault) {
        if let Ok(mut slot) = self.fault.try_borrow_mut() {
            if slot.is_none() {
                *slot = Some(fault);
            }
        }
    }

    fn active(&self) -> bool {
        self.fault.try_borrow().is_ok_and(|fault| fault.is_none())
    }
}

/// Copy-only raw capability for synchronous reentry into the currently
/// executing VM. It never constructs a second Rust reference to `Dtvm`.
#[derive(Clone, Copy)]
pub(crate) struct ReentrantVm {
    pub(crate) vm: NonNull<EvmcVm>,
    pub(crate) execute: ExecuteFn,
    pub(crate) revision: i32,
}

/// One stack-local EVMC context.
///
/// Recursive calls create a distinct value sharing this session instead of
/// reusing the parent's context pointer. The session remains single-threaded
/// and all mutable backend borrows are short `RefCell` borrows that end before
/// the VM is synchronously reentered.
pub struct HostContext<'a> {
    session: Rc<HostSession<'a>>,
}

impl<'a> HostContext<'a> {
    pub fn new(backend: &'a mut dyn HostBackend, tx_context: TxContextOwned) -> Self {
        Self {
            session: Rc::new(HostSession {
                state: RefCell::new(HostState {
                    backend,
                    audit: Vec::new(),
                }),
                fault: RefCell::new(None),
                tx_context,
                reentrant_vm: Cell::new(None),
            }),
        }
    }

    fn child(session: Rc<HostSession<'a>>) -> Self {
        Self { session }
    }

    pub(crate) fn install_reentrant_vm(&self, vm: ReentrantVm) {
        self.session.reentrant_vm.set(Some(vm));
    }

    pub(crate) fn clear_reentrant_vm(&self) {
        self.session.reentrant_vm.set(None);
    }

    pub fn fault(&self) -> Option<HostFault> {
        self.session.fault.borrow().clone()
    }

    pub fn take_fault(&mut self) -> Option<HostFault> {
        self.session.fault.borrow_mut().take()
    }

    pub fn audit(&self) -> Vec<AccessEvent> {
        self.session.state.borrow().audit.clone()
    }

    pub fn begin_frame(&mut self) -> Result<(), HostFault> {
        let mut state = self.session.state.try_borrow_mut().map_err(|_| {
            HostFault::Backend("host session was already borrowed at root begin".into())
        })?;
        *self.session.fault.borrow_mut() = None;
        state.audit.clear();
        state.backend.checkpoint()
    }

    pub fn commit_frame(&mut self) -> Result<(), HostFault> {
        if let Some(fault) = self.fault() {
            return Err(fault);
        }
        self.session
            .state
            .try_borrow_mut()
            .map_err(|_| {
                HostFault::Backend("host session was already borrowed at root commit".into())
            })?
            .backend
            .commit_checkpoint()
    }

    pub fn revert_frame(&mut self) -> Result<(), HostFault> {
        self.session
            .state
            .try_borrow_mut()
            .map_err(|_| {
                HostFault::Backend("host session was already borrowed at root revert".into())
            })?
            .backend
            .revert_checkpoint()
    }

    /// Validates state that enters DTVM through the independent `code`
    /// argument rather than an EVMC callback.
    pub fn validate_top_level(
        &mut self,
        recipient: Address,
        code_address: Address,
        code: &[u8],
    ) -> Result<(), HostFault> {
        let mut state = self.session.state.try_borrow_mut().map_err(|_| {
            HostFault::Backend("host session was already borrowed during root validation".into())
        })?;
        // The Reth transaction or system-call shell owns caller validation and
        // warming. This VM boundary validates only the execution/storage
        // participant and the authoritative code provider.
        for address in [recipient, code_address] {
            state.audit.push(AccessEvent::AccountExists(address));
            if !state.backend.is_account_warm(address)? {
                return Err(HostFault::TopLevelAccountCold(address));
            }
            if !state.backend.account_exists(address)? {
                return Err(HostFault::TopLevelAccountAbsent(address));
            }
        }
        state.audit.push(AccessEvent::CodeSize(code_address));
        state.audit.push(AccessEvent::CodeHash(code_address));
        state
            .audit
            .push(AccessEvent::CodeCopy(code_address, 0, code.len()));
        let (authoritative, _) = state.checked_code_and_hash(code_address)?;
        if authoritative != code {
            return Err(HostFault::CodeMismatch(code_address));
        }
        Ok(())
    }

    /// Validates the Reth-initialized account that receives a top-level
    /// CREATE initcode frame. Its runtime code does not exist until Reth
    /// processes the successful frame result, so this boundary deliberately
    /// does not compare the initcode against account code.
    pub fn validate_top_level_create(&mut self, recipient: Address) -> Result<(), HostFault> {
        let mut state = self.session.state.try_borrow_mut().map_err(|_| {
            HostFault::Backend(
                "host session was already borrowed during root CREATE validation".into(),
            )
        })?;
        state.audit.push(AccessEvent::AccountExists(recipient));
        if !state.backend.is_account_warm(recipient)? {
            return Err(HostFault::TopLevelAccountCold(recipient));
        }
        if !state.backend.account_exists(recipient)? {
            return Err(HostFault::TopLevelAccountAbsent(recipient));
        }
        Ok(())
    }
}

unsafe fn with_context<R>(
    context: *mut HostContextOpaque,
    callback: &'static str,
    default: R,
    body: impl FnOnce(&mut HostState<'_>, &TxContextOwned) -> Result<R, HostFault>,
) -> R {
    if context.is_null() {
        return default;
    }
    // SAFETY: each layer passes its distinct live stack-local HostContext. The
    // `'static` cast only erases the backend lifetime across the C ABI; the Rc
    // clone stays inside this synchronous callback and cannot escape the outer
    // root execute whose stack owns the backend.
    let host = unsafe { &*(context.cast::<HostContext<'static>>()) };
    let session = Rc::clone(&host.session);
    if !session.active() {
        return default;
    }
    let mut state = match session.state.try_borrow_mut() {
        Ok(state) => state,
        Err(_) => {
            session.latch(HostFault::Backend(format!(
                "host session remained borrowed during {callback}"
            )));
            return default;
        }
    };
    match catch_unwind(AssertUnwindSafe(|| body(&mut state, &session.tx_context))) {
        Ok(Ok(value)) => value,
        Ok(Err(fault)) => {
            session.latch(fault);
            default
        }
        Err(_) => {
            session.latch(HostFault::CallbackPanicked { callback });
            default
        }
    }
}

unsafe fn read_address(
    value: *const EvmcAddress,
    what: &'static str,
) -> Result<Address, HostFault> {
    if value.is_null() {
        Err(HostFault::InvalidPointer { what })
    } else {
        // SAFETY: EVMC guarantees a valid pointer for callback arguments.
        Ok(unsafe { *value }.into())
    }
}

unsafe fn read_word(value: *const EvmcBytes32, what: &'static str) -> Result<Word, HostFault> {
    if value.is_null() {
        Err(HostFault::InvalidPointer { what })
    } else {
        // SAFETY: EVMC guarantees a valid pointer for callback arguments.
        Ok(unsafe { *value }.into())
    }
}

unsafe extern "C" fn account_exists(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
) -> bool {
    unsafe {
        with_context(context, "account_exists", false, |host, _| {
            let address = read_address(address, "account_exists.address")?;
            host.audit.push(AccessEvent::AccountExists(address));
            host.backend.account_exists(address)
        })
    }
}

unsafe extern "C" fn get_storage(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
    key: *const EvmcBytes32,
) -> EvmcBytes32 {
    unsafe {
        with_context(context, "get_storage", EvmcBytes32::default(), |host, _| {
            let address = read_address(address, "get_storage.address")?;
            let key = read_word(key, "get_storage.key")?;
            host.audit.push(AccessEvent::StorageRead(address, key));
            host.backend.get_storage(address, key).map(Into::into)
        })
    }
}

unsafe extern "C" fn set_storage(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
    key: *const EvmcBytes32,
    value: *const EvmcBytes32,
) -> c_int {
    unsafe {
        with_context(context, "set_storage", EVMC_STORAGE_ASSIGNED, |host, _| {
            let address = read_address(address, "set_storage.address")?;
            let key = read_word(key, "set_storage.key")?;
            let value = read_word(value, "set_storage.value")?;
            host.audit
                .push(AccessEvent::StorageWrite(address, key, value));
            host.backend.set_storage(address, key, value)
        })
    }
}

unsafe extern "C" fn get_balance(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
) -> EvmcBytes32 {
    unsafe {
        with_context(context, "get_balance", EvmcBytes32::default(), |host, _| {
            let address = read_address(address, "get_balance.address")?;
            host.audit.push(AccessEvent::Balance(address));
            host.backend.get_balance(address).map(Into::into)
        })
    }
}

unsafe extern "C" fn get_code_size(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
) -> usize {
    unsafe {
        with_context(context, "get_code_size", 0, |host, _| {
            let address = read_address(address, "get_code_size.address")?;
            host.audit.push(AccessEvent::CodeSize(address));
            host.audit.push(AccessEvent::AccountExists(address));
            host.checked_code_and_hash(address)
                .map(|(code, _)| code.len())
        })
    }
}

unsafe extern "C" fn get_code_hash(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
) -> EvmcBytes32 {
    unsafe {
        with_context(
            context,
            "get_code_hash",
            EvmcBytes32::default(),
            |host, _| {
                let address = read_address(address, "get_code_hash.address")?;
                host.audit.push(AccessEvent::CodeHash(address));
                host.audit.push(AccessEvent::AccountExists(address));
                host.checked_code_and_hash(address)
                    .map(|(_, code_hash)| code_hash.into())
            },
        )
    }
}

unsafe extern "C" fn copy_code(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
    code_offset: usize,
    buffer_data: *mut u8,
    buffer_size: usize,
) -> usize {
    unsafe {
        with_context(context, "copy_code", 0, |host, _| {
            let address = read_address(address, "copy_code.address")?;
            if buffer_size != 0 && buffer_data.is_null() {
                return Err(HostFault::InvalidPointer {
                    what: "copy_code.buffer",
                });
            }
            host.audit
                .push(AccessEvent::CodeCopy(address, code_offset, buffer_size));
            host.audit.push(AccessEvent::AccountExists(address));
            let (code, _) = host.checked_code_and_hash(address)?;
            let available = code.len().saturating_sub(code_offset);
            let count = available.min(buffer_size);
            if count != 0 {
                // SAFETY: the VM owns a writable buffer of `buffer_size`.
                ptr::copy_nonoverlapping(code.as_ptr().add(code_offset), buffer_data, count);
            }
            Ok(count)
        })
    }
}

unsafe extern "C" fn selfdestruct(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
    beneficiary: *const EvmcAddress,
) -> bool {
    unsafe {
        with_context(context, "selfdestruct", false, |host, _| {
            let address = read_address(address, "selfdestruct.address")?;
            let beneficiary = read_address(beneficiary, "selfdestruct.beneficiary")?;
            host.audit
                .push(AccessEvent::Selfdestruct(address, beneficiary));
            host.backend.selfdestruct(address, beneficiary)
        })
    }
}

unsafe extern "C" fn call(
    context: *mut HostContextOpaque,
    message: *const EvmcMessage,
) -> EvmcResult {
    if context.is_null() {
        return EvmcResult::failure();
    }
    let result = catch_unwind(AssertUnwindSafe(|| unsafe { call_inner(context, message) }));
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(fault)) => {
            unsafe { latch_context_fault(context, fault) };
            EvmcResult::failure()
        }
        Err(_) => {
            unsafe {
                latch_context_fault(context, HostFault::CallbackPanicked { callback: "call" })
            };
            EvmcResult::failure()
        }
    }
}

unsafe fn call_inner(
    context: *mut HostContextOpaque,
    message: *const EvmcMessage,
) -> Result<EvmcResult, HostFault> {
    if message.is_null() {
        return Err(HostFault::InvalidPointer {
            what: "call.message",
        });
    }
    // SAFETY: the caller checked the pointer and EVMC keeps the message live
    // for this synchronous callback.
    let raw = unsafe { *message };
    let input = unsafe { read_bytes(raw.input_data, raw.input_size, "call.message.input")? };
    let gas = u64::try_from(raw.gas).map_err(|_| HostFault::NestedCallGasInvalid(raw.gas))?;
    let request = NestedCallRequest {
        kind: raw.kind,
        flags: raw.flags,
        depth: raw.depth,
        gas,
        recipient: raw.recipient.into(),
        sender: raw.sender.into(),
        input,
        value: raw.value.into(),
        create2_salt: raw.create2_salt.into(),
        code_address: raw.code_address.into(),
    };

    // SAFETY: every raw pointer passed to this callback points to the current
    // layer's live stack-local HostContext. The `'static` cast is lifetime
    // erasure across the C ABI only: this private Rc clone is consumed by the
    // synchronous child execute/finish sequence and cannot escape root execute.
    // Cloning it does not borrow mutable backend state.
    let session = unsafe { Rc::clone(&(*(context.cast::<HostContext<'static>>())).session) };
    let preparation = {
        if !session.active() {
            return Ok(EvmcResult::failure());
        }
        let mut state = session.state.try_borrow_mut().map_err(|_| {
            let fault =
                HostFault::Backend("host session remained borrowed during nested prepare".into());
            session.latch(fault.clone());
            fault
        })?;
        state.audit.push(AccessEvent::NestedCall(request.kind));
        validate_nested_request(&request)?;
        state.backend.begin_call(&request)?
    };

    let NestedCallPreparation::Execute {
        code,
        recipient,
        input,
    } = preparation
    else {
        let NestedCallPreparation::Immediate(result) = preparation else {
            unreachable!()
        };
        return Ok(owned_evmc_result(result));
    };
    let mut execution_request = request;
    execution_request.recipient = recipient;
    execution_request.input = input;

    let vm = match session.reentrant_vm.get() {
        Some(vm) => vm,
        None => {
            finish_failed_call(&session)?;
            return Err(HostFault::Backend(
                "nested CALL reached a host without an active DTVM session".into(),
            ));
        }
    };
    let child_result =
        unsafe { execute_reentrant(vm, &execution_request, &code, Rc::clone(&session)) };

    let result = match child_result {
        Ok(result) if session.active() => {
            let mut state = session.state.try_borrow_mut().map_err(|_| {
                let fault = HostFault::Backend(
                    "host session remained borrowed during nested finish".into(),
                );
                session.latch(fault.clone());
                fault
            })?;
            state.backend.finish_call(result)?
        }
        Ok(_) => {
            finish_failed_call(&session)?;
            return Ok(EvmcResult::failure());
        }
        Err(fault) => {
            finish_failed_call(&session)?;
            return Err(fault);
        }
    };
    Ok(owned_evmc_result(result))
}

fn validate_nested_request(request: &NestedCallRequest) -> Result<(), HostFault> {
    match request.kind {
        EVMC_CALL | EVMC_DELEGATECALL | EVMC_CALLCODE => {
            if request.flags & !(EVMC_STATIC | EVMC_DELEGATED) != 0 {
                return Err(HostFault::NestedCallFlagsUnsupported {
                    flags: request.flags,
                });
            }
            if request.kind == EVMC_CALL
                && request.flags & EVMC_DELEGATED == 0
                && request.code_address != request.recipient
            {
                return Err(HostFault::NestedCallCodeAddressMismatch);
            }
        }
        EVMC_CREATE | EVMC_CREATE2 => {
            if request.flags & !EVMC_DELEGATED != 0 {
                return Err(HostFault::NestedCallFlagsUnsupported {
                    flags: request.flags,
                });
            }
        }
        kind => return Err(HostFault::NestedCallUnsupported { kind }),
    }
    if request.depth < 0 {
        return Err(HostFault::NestedCallDepthInvalid(request.depth));
    }
    Ok(())
}

fn finish_failed_call(session: &Rc<HostSession<'_>>) -> Result<(), HostFault> {
    let mut state = session.state.try_borrow_mut().map_err(|_| {
        let fault =
            HostFault::Backend("host session remained borrowed while aborting nested CALL".into());
        session.latch(fault.clone());
        fault
    })?;
    state
        .backend
        .finish_call(NestedCallResult {
            status_code: EVMC_FAILURE,
            gas_left: 0,
            gas_refund: 0,
            output: Vec::new(),
            create_address: None,
        })
        .map(|_| ())
}

unsafe fn execute_reentrant(
    vm: ReentrantVm,
    request: &NestedCallRequest,
    code: &[u8],
    session: Rc<HostSession<'static>>,
) -> Result<NestedCallResult, HostFault> {
    let gas = i64::try_from(request.gas).map_err(|_| HostFault::NestedCallGasInvalid(i64::MAX))?;
    let raw_message = EvmcMessage {
        kind: request.kind,
        flags: request.flags,
        depth: request.depth,
        gas,
        recipient: request.recipient.into(),
        sender: request.sender.into(),
        input_data: null_if_empty(&request.input),
        input_size: request.input.len(),
        value: request.value.into(),
        create2_salt: request.create2_salt.into(),
        code_address: request.code_address.into(),
        code: null_if_empty(code),
        code_size: code.len(),
    };
    let mut child_host = HostContext::child(session);
    // SAFETY: `ReentrantVm` is installed by the outer live `Dtvm`; no Rust
    // reference to that object is created here. `child_host`, message, and code
    // remain live for this complete synchronous call.
    let result = unsafe {
        (vm.execute)(
            vm.vm.as_ptr(),
            &HOST_INTERFACE,
            (&mut child_host as *mut HostContext<'_>).cast(),
            vm.revision,
            &raw_message,
            null_if_empty(code),
            code.len(),
        )
    };
    let result = HostResultGuard(result);
    if result.0.output_size != 0 && result.0.output_data.is_null() {
        return Err(HostFault::Backend(
            "nested DTVM returned non-empty output with a null pointer".into(),
        ));
    }
    if result.0.gas_left < 0 || result.0.gas_left > gas {
        return Err(HostFault::Backend(
            "nested DTVM returned gas outside the child frame limit".into(),
        ));
    }
    if !is_consensus_status(result.0.status_code) {
        return Err(HostFault::Backend(format!(
            "nested DTVM returned backend/internal status {}",
            result.0.status_code
        )));
    }
    match result.0.status_code {
        EVMC_SUCCESS => {}
        EVMC_REVERT if result.0.gas_refund == 0 => {}
        EVMC_REVERT => {
            return Err(HostFault::Backend(
                "nested DTVM REVERT returned a nonzero refund".into(),
            ));
        }
        _ if result.0.gas_left == 0 && result.0.gas_refund == 0 => {}
        _ => {
            return Err(HostFault::Backend(
                "nested DTVM halt retained gas or refund".into(),
            ));
        }
    }
    let output = unsafe {
        read_bytes(
            result.0.output_data,
            result.0.output_size,
            "call.result.output",
        )?
    };
    Ok(NestedCallResult {
        status_code: result.0.status_code,
        gas_left: result.0.gas_left as u64,
        gas_refund: result.0.gas_refund,
        output,
        create_address: None,
    })
}

fn is_consensus_status(status: i32) -> bool {
    matches!(
        status,
        EVMC_SUCCESS
            | EVMC_REVERT
            | EVMC_OUT_OF_GAS
            | EVMC_INVALID_INSTRUCTION
            | EVMC_UNDEFINED_INSTRUCTION
            | EVMC_STACK_OVERFLOW
            | EVMC_STACK_UNDERFLOW
            | EVMC_BAD_JUMP_DESTINATION
            | EVMC_INVALID_MEMORY_ACCESS
            | EVMC_CALL_DEPTH_EXCEEDED
            | EVMC_STATIC_MODE_VIOLATION
            | EVMC_PRECOMPILE_FAILURE
            | EVMC_CONTRACT_VALIDATION_FAILURE
            | EVMC_ARGUMENT_OUT_OF_RANGE
            | EVMC_INSUFFICIENT_BALANCE
    )
}

unsafe fn latch_context_fault(context: *mut HostContextOpaque, fault: HostFault) {
    if context.is_null() {
        return;
    }
    // SAFETY: callback context is live for the enclosing synchronous execute.
    let host = unsafe { &*(context.cast::<HostContext<'static>>()) };
    host.session.latch(fault);
}

unsafe fn read_bytes(
    data: *const u8,
    size: usize,
    what: &'static str,
) -> Result<Vec<u8>, HostFault> {
    if size == 0 {
        return Ok(Vec::new());
    }
    if data.is_null() || size > isize::MAX as usize {
        return Err(HostFault::InvalidPointer { what });
    }
    // SAFETY: EVMC owns a readable buffer of `size` bytes for the callback.
    Ok(unsafe { slice::from_raw_parts(data, size) }.to_vec())
}

fn owned_evmc_result(result: NestedCallResult) -> EvmcResult {
    let (output_data, output_size, release) = if result.output.is_empty() {
        (ptr::null(), 0, None)
    } else {
        let output = result.output.into_boxed_slice();
        let output_size = output.len();
        let output_data = Box::into_raw(output) as *mut u8;
        (
            output_data.cast_const(),
            output_size,
            Some(release_owned_output as crate::ffi::EvmcReleaseResultFn),
        )
    };
    EvmcResult {
        status_code: result.status_code,
        gas_left: i64::try_from(result.gas_left).unwrap_or_default(),
        gas_refund: result.gas_refund,
        output_data,
        output_size,
        release,
        create_address: result.create_address.unwrap_or_default().into(),
        padding: [0; 4],
    }
}

unsafe extern "C" fn release_owned_output(result: *const EvmcResult) {
    if result.is_null() {
        return;
    }
    // SAFETY: `owned_evmc_result` created this exact allocation and EVMC
    // invokes release at most once for the returned result.
    let result = unsafe { &*result };
    if result.output_size != 0 && !result.output_data.is_null() {
        let output =
            ptr::slice_from_raw_parts_mut(result.output_data.cast_mut(), result.output_size);
        // SAFETY: pointer and length reconstruct the original Box<[u8]>.
        drop(unsafe { Box::from_raw(output) });
    }
}

struct HostResultGuard(EvmcResult);

impl Drop for HostResultGuard {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            // SAFETY: release belongs to this exact result and is invoked once.
            unsafe { release(&self.0) };
        }
    }
}

unsafe extern "C" fn get_tx_context(context: *mut HostContextOpaque) -> EvmcTxContext {
    unsafe {
        with_context(
            context,
            "get_tx_context",
            EvmcTxContext::default(),
            |host, tx_context| {
                host.audit.push(AccessEvent::TxContext);
                Ok(tx_context.raw())
            },
        )
    }
}

unsafe extern "C" fn get_block_hash(context: *mut HostContextOpaque, number: i64) -> EvmcBytes32 {
    unsafe {
        with_context(
            context,
            "get_block_hash",
            EvmcBytes32::default(),
            |host, _| {
                host.audit.push(AccessEvent::BlockHash(number));
                host.backend.block_hash(number).map(Into::into)
            },
        )
    }
}

unsafe extern "C" fn emit_log(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
    data: *const u8,
    data_size: usize,
    topics: *const EvmcBytes32,
    topics_count: usize,
) {
    unsafe {
        with_context(context, "emit_log", (), |host, _| {
            let address = read_address(address, "emit_log.address")?;
            if topics_count > 4 {
                return Err(HostFault::InvalidTopicCount(topics_count));
            }
            if data_size != 0 && data.is_null() {
                return Err(HostFault::InvalidPointer {
                    what: "emit_log.data",
                });
            }
            if data_size > isize::MAX as usize {
                return Err(HostFault::InvalidPointer {
                    what: "emit_log.data_size",
                });
            }
            if topics_count != 0 && topics.is_null() {
                return Err(HostFault::InvalidPointer {
                    what: "emit_log.topics",
                });
            }
            let data = if data_size == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(data, data_size).to_vec()
            };
            let topics = if topics_count == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(topics, topics_count)
                    .iter()
                    .copied()
                    .map(Into::into)
                    .collect()
            };
            host.audit
                .push(AccessEvent::Log(address, data_size, topics_count));
            host.backend.emit_log(LogEntry {
                address,
                data,
                topics,
            })
        });
    }
}

unsafe extern "C" fn access_account(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
) -> c_int {
    unsafe {
        with_context(context, "access_account", EVMC_ACCESS_COLD, |host, _| {
            let address = read_address(address, "access_account.address")?;
            host.audit.push(AccessEvent::AccountWarm(address));
            host.backend.access_account(address).map(|was_warm| {
                if was_warm {
                    EVMC_ACCESS_WARM
                } else {
                    EVMC_ACCESS_COLD
                }
            })
        })
    }
}

unsafe extern "C" fn access_storage(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
    key: *const EvmcBytes32,
) -> c_int {
    unsafe {
        with_context(context, "access_storage", EVMC_ACCESS_COLD, |host, _| {
            let address = read_address(address, "access_storage.address")?;
            let key = read_word(key, "access_storage.key")?;
            host.audit.push(AccessEvent::StorageWarm(address, key));
            host.backend.access_storage(address, key).map(|was_warm| {
                if was_warm {
                    EVMC_ACCESS_WARM
                } else {
                    EVMC_ACCESS_COLD
                }
            })
        })
    }
}

unsafe extern "C" fn get_transient_storage(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
    key: *const EvmcBytes32,
) -> EvmcBytes32 {
    unsafe {
        with_context(
            context,
            "get_transient_storage",
            EvmcBytes32::default(),
            |host, _| {
                let address = read_address(address, "get_transient_storage.address")?;
                let key = read_word(key, "get_transient_storage.key")?;
                host.audit.push(AccessEvent::TransientRead(address, key));
                host.backend
                    .get_transient_storage(address, key)
                    .map(Into::into)
            },
        )
    }
}

unsafe extern "C" fn set_transient_storage(
    context: *mut HostContextOpaque,
    address: *const EvmcAddress,
    key: *const EvmcBytes32,
    value: *const EvmcBytes32,
) {
    unsafe {
        with_context(context, "set_transient_storage", (), |host, _| {
            let address = read_address(address, "set_transient_storage.address")?;
            let key = read_word(key, "set_transient_storage.key")?;
            let value = read_word(value, "set_transient_storage.value")?;
            host.audit
                .push(AccessEvent::TransientWrite(address, key, value));
            host.backend.set_transient_storage(address, key, value)
        });
    }
}

pub static HOST_INTERFACE: EvmcHostInterface = EvmcHostInterface {
    account_exists,
    get_storage,
    set_storage,
    get_balance,
    get_code_size,
    get_code_hash,
    copy_code,
    selfdestruct,
    call,
    get_tx_context,
    get_block_hash,
    emit_log,
    access_account,
    access_storage,
    get_transient_storage,
    set_transient_storage,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn w(value: u64) -> Word {
        Word::from_u64(value)
    }

    #[derive(Default)]
    struct FailingChildBackend {
        root_depth: usize,
        root_commits: usize,
        root_reverts: usize,
        child_active: bool,
        child_failures: usize,
    }

    impl FailingChildBackend {
        fn unused<T>() -> Result<T, HostFault> {
            Err(HostFault::Backend("unused fake backend callback".into()))
        }
    }

    impl HostBackend for FailingChildBackend {
        fn checkpoint(&mut self) -> Result<(), HostFault> {
            self.root_depth += 1;
            Ok(())
        }

        fn commit_checkpoint(&mut self) -> Result<(), HostFault> {
            self.root_depth = self
                .root_depth
                .checked_sub(1)
                .ok_or_else(|| HostFault::Backend("fake root commit without checkpoint".into()))?;
            self.root_commits += 1;
            Ok(())
        }

        fn revert_checkpoint(&mut self) -> Result<(), HostFault> {
            self.root_depth = self
                .root_depth
                .checked_sub(1)
                .ok_or_else(|| HostFault::Backend("fake root revert without checkpoint".into()))?;
            self.root_reverts += 1;
            Ok(())
        }

        fn account_exists(&mut self, _address: Address) -> Result<bool, HostFault> {
            Self::unused()
        }

        fn get_storage(&mut self, _address: Address, _key: Word) -> Result<Word, HostFault> {
            Self::unused()
        }

        fn set_storage(
            &mut self,
            _address: Address,
            _key: Word,
            _value: Word,
        ) -> Result<i32, HostFault> {
            Self::unused()
        }

        fn get_balance(&mut self, _address: Address) -> Result<Word, HostFault> {
            Self::unused()
        }

        fn get_code(&mut self, _address: Address) -> Result<Vec<u8>, HostFault> {
            Self::unused()
        }

        fn get_code_hash(&mut self, _address: Address) -> Result<Word, HostFault> {
            Self::unused()
        }

        fn selfdestruct(
            &mut self,
            _address: Address,
            _beneficiary: Address,
        ) -> Result<bool, HostFault> {
            Self::unused()
        }

        fn block_hash(&mut self, _number: i64) -> Result<Word, HostFault> {
            Self::unused()
        }

        fn emit_log(&mut self, _log: LogEntry) -> Result<(), HostFault> {
            Self::unused()
        }

        fn access_account(&mut self, _address: Address) -> Result<bool, HostFault> {
            Self::unused()
        }

        fn is_account_warm(&mut self, _address: Address) -> Result<bool, HostFault> {
            Self::unused()
        }

        fn access_storage(&mut self, _address: Address, _key: Word) -> Result<bool, HostFault> {
            Self::unused()
        }

        fn get_transient_storage(
            &mut self,
            _address: Address,
            _key: Word,
        ) -> Result<Word, HostFault> {
            Self::unused()
        }

        fn set_transient_storage(
            &mut self,
            _address: Address,
            _key: Word,
            _value: Word,
        ) -> Result<(), HostFault> {
            Self::unused()
        }

        fn begin_call(
            &mut self,
            request: &NestedCallRequest,
        ) -> Result<NestedCallPreparation, HostFault> {
            if self.child_active {
                return Err(HostFault::Backend(
                    "fake child checkpoint already active".into(),
                ));
            }
            self.child_active = true;
            Ok(NestedCallPreparation::Execute {
                code: vec![0x00],
                recipient: request.recipient,
                input: request.input.clone(),
            })
        }

        fn finish_call(&mut self, result: NestedCallResult) -> Result<NestedCallResult, HostFault> {
            if !self.child_active {
                return Err(HostFault::Backend(
                    "fake child finish without checkpoint".into(),
                ));
            }
            self.child_active = false;
            if result.status_code == EVMC_FAILURE {
                self.child_failures += 1;
            }
            Ok(result)
        }
    }

    unsafe extern "C" fn failing_child_execute(
        _vm: *mut EvmcVm,
        _host: *const EvmcHostInterface,
        _context: *mut HostContextOpaque,
        _revision: c_int,
        _message: *const EvmcMessage,
        _code: *const u8,
        _code_size: usize,
    ) -> EvmcResult {
        EvmcResult::failure()
    }

    #[test]
    fn storage_status_matrix_matches_evmc_abi() {
        let cases = [
            ((0, 0, 0), EVMC_STORAGE_ASSIGNED),
            ((7, 7, 7), EVMC_STORAGE_ASSIGNED),
            ((0, 9, 8), EVMC_STORAGE_ASSIGNED),
            ((7, 9, 8), EVMC_STORAGE_ASSIGNED),
            ((0, 0, 9), EVMC_STORAGE_ADDED),
            ((7, 7, 0), EVMC_STORAGE_DELETED),
            ((7, 7, 9), EVMC_STORAGE_MODIFIED),
            ((7, 0, 9), EVMC_STORAGE_DELETED_ADDED),
            ((7, 9, 0), EVMC_STORAGE_MODIFIED_DELETED),
            ((7, 0, 7), EVMC_STORAGE_DELETED_RESTORED),
            ((0, 9, 0), EVMC_STORAGE_ADDED_DELETED),
            ((7, 9, 7), EVMC_STORAGE_MODIFIED_RESTORED),
        ];
        for ((original, present, new), expected) in cases {
            assert_eq!(
                classify_storage_status(w(original), w(present), w(new)),
                expected,
                "triple {original} -> {present} -> {new}"
            );
        }
    }

    #[test]
    fn missing_witness_is_sticky_and_frame_reverts() {
        let covered = Address([1; 20]);
        let missing = Address([2; 20]);
        let key = w(3);
        let mut backend = WitnessBackend::default();
        backend.insert_account(covered, Account::new(Word::ZERO, Vec::new()));
        backend.cover_storage(covered, key, w(4)).unwrap();
        let mut host = HostContext::new(&mut backend, TxContextOwned::default());
        host.begin_frame().unwrap();

        let covered_ffi: EvmcAddress = covered.into();
        let missing_ffi: EvmcAddress = missing.into();
        let key_ffi: EvmcBytes32 = key.into();
        let new_ffi: EvmcBytes32 = w(5).into();
        unsafe {
            assert_eq!(
                (HOST_INTERFACE.set_storage)(
                    (&mut host as *mut HostContext<'_>).cast(),
                    &covered_ffi,
                    &key_ffi,
                    &new_ffi,
                ),
                EVMC_STORAGE_MODIFIED
            );
            assert!(!(HOST_INTERFACE.account_exists)(
                (&mut host as *mut HostContext<'_>).cast(),
                &missing_ffi,
            ));
        }
        assert_eq!(host.fault(), Some(HostFault::MissingAccount(missing)));
        host.revert_frame().unwrap();
        drop(host);
        assert_eq!(
            backend
                .account(covered)
                .unwrap()
                .present_storage(key)
                .unwrap(),
            w(4)
        );
    }

    #[test]
    fn nested_call_latches_fault() {
        let mut backend = WitnessBackend::default();
        let mut host = HostContext::new(&mut backend, TxContextOwned::default());
        host.begin_frame().unwrap();
        let message = EvmcMessage {
            kind: 1,
            flags: 0,
            depth: 1,
            gas: 1,
            recipient: EvmcAddress::default(),
            sender: EvmcAddress::default(),
            input_data: ptr::null(),
            input_size: 0,
            value: EvmcBytes32::default(),
            create2_salt: EvmcBytes32::default(),
            code_address: EvmcAddress::default(),
            code: ptr::null(),
            code_size: 0,
        };
        let result =
            unsafe { (HOST_INTERFACE.call)((&mut host as *mut HostContext<'_>).cast(), &message) };
        assert_eq!(result.status_code, crate::ffi::EVMC_FAILURE);
        assert_eq!(
            host.fault(),
            Some(HostFault::NestedCallUnsupported { kind: 1 })
        );
        host.revert_frame().unwrap();
    }

    #[test]
    fn nested_create_flags_allow_delegated_but_reject_static_and_eofcreate() {
        let request = NestedCallRequest {
            kind: EVMC_CREATE2,
            flags: 0,
            depth: 1,
            gas: 1,
            recipient: Address::default(),
            sender: Address([0x11; 20]),
            input: vec![0x00],
            value: Word::ZERO,
            create2_salt: Word::from_u64(7),
            code_address: Address::default(),
        };
        assert_eq!(validate_nested_request(&request), Ok(()));
        assert_eq!(
            validate_nested_request(&NestedCallRequest {
                flags: EVMC_DELEGATED,
                ..request.clone()
            }),
            Ok(())
        );
        assert_eq!(
            validate_nested_request(&NestedCallRequest {
                flags: EVMC_STATIC,
                ..request.clone()
            }),
            Err(HostFault::NestedCallFlagsUnsupported { flags: EVMC_STATIC })
        );
        assert_eq!(
            validate_nested_request(&NestedCallRequest {
                kind: crate::ffi::EVMC_EOFCREATE,
                ..request
            }),
            Err(HostFault::NestedCallUnsupported {
                kind: crate::ffi::EVMC_EOFCREATE,
            })
        );
    }

    #[test]
    fn nested_call_flags_allow_static_and_delegated_for_all_call_kinds() {
        let request = NestedCallRequest {
            kind: EVMC_CALL,
            flags: 0,
            depth: 1,
            gas: 1,
            recipient: Address([0x11; 20]),
            sender: Address([0x22; 20]),
            input: Vec::new(),
            value: Word::ZERO,
            create2_salt: Word::ZERO,
            code_address: Address([0x11; 20]),
        };
        for kind in [EVMC_CALL, EVMC_DELEGATECALL, EVMC_CALLCODE] {
            for flags in [0, EVMC_STATIC, EVMC_DELEGATED, EVMC_STATIC | EVMC_DELEGATED] {
                assert_eq!(
                    validate_nested_request(&NestedCallRequest {
                        kind,
                        flags,
                        ..request.clone()
                    }),
                    Ok(())
                );
            }
        }
        assert_eq!(
            validate_nested_request(&NestedCallRequest {
                flags: 4,
                ..request.clone()
            }),
            Err(HostFault::NestedCallFlagsUnsupported { flags: 4 })
        );
        assert_eq!(
            validate_nested_request(&NestedCallRequest {
                code_address: Address([0x33; 20]),
                ..request.clone()
            }),
            Err(HostFault::NestedCallCodeAddressMismatch)
        );
        assert_eq!(
            validate_nested_request(&NestedCallRequest {
                flags: EVMC_DELEGATED,
                code_address: Address([0x33; 20]),
                ..request
            }),
            Ok(())
        );
    }

    #[test]
    fn nested_backend_failure_is_sticky_and_cannot_commit_root() {
        let mut backend = FailingChildBackend::default();
        let mut host = HostContext::new(&mut backend, TxContextOwned::default());
        host.begin_frame().unwrap();
        let mut raw_vm = EvmcVm {
            abi_version: crate::ffi::EVMC_ABI_VERSION,
            name: ptr::null(),
            version: ptr::null(),
            destroy: None,
            execute: Some(failing_child_execute),
            get_capabilities: None,
            set_option: None,
        };
        host.install_reentrant_vm(ReentrantVm {
            vm: NonNull::from(&mut raw_vm),
            execute: failing_child_execute,
            revision: crate::ffi::EVMC_OSAKA,
        });
        let message = EvmcMessage {
            kind: EVMC_CALL,
            flags: 0,
            depth: 1,
            gas: 100,
            recipient: EvmcAddress::default(),
            sender: EvmcAddress::default(),
            input_data: ptr::null(),
            input_size: 0,
            value: EvmcBytes32::default(),
            create2_salt: EvmcBytes32::default(),
            code_address: EvmcAddress::default(),
            code: ptr::null(),
            code_size: 0,
        };

        let result =
            unsafe { (HOST_INTERFACE.call)((&mut host as *mut HostContext<'_>).cast(), &message) };
        assert_eq!(result.status_code, EVMC_FAILURE);
        let fault = host.fault().expect("backend failure must be sticky");
        assert!(matches!(
            &fault,
            HostFault::Backend(message)
                if message.contains("backend/internal status 1")
        ));
        assert_eq!(host.commit_frame(), Err(fault));
        host.revert_frame().unwrap();
        drop(host);

        assert_eq!(backend.root_commits, 0);
        assert_eq!(backend.root_reverts, 1);
        assert_eq!(backend.root_depth, 0);
        assert_eq!(backend.child_failures, 1);
        assert!(!backend.child_active);
    }

    #[test]
    fn nested_delegated_flag_is_forwarded_but_unknown_flags_and_plain_mismatch_fail_closed() {
        let cases = [
            (
                1 << 2,
                EvmcAddress::default(),
                HostFault::NestedCallFlagsUnsupported { flags: 1 << 2 },
            ),
            (
                0,
                EvmcAddress { bytes: [1; 20] },
                HostFault::NestedCallCodeAddressMismatch,
            ),
        ];
        for (flags, code_address, expected) in cases {
            let mut backend = WitnessBackend::default();
            let mut host = HostContext::new(&mut backend, TxContextOwned::default());
            host.begin_frame().unwrap();
            let message = EvmcMessage {
                kind: EVMC_CALL,
                flags,
                depth: 1,
                gas: 1,
                recipient: EvmcAddress::default(),
                sender: EvmcAddress::default(),
                input_data: ptr::null(),
                input_size: 0,
                value: EvmcBytes32::default(),
                create2_salt: EvmcBytes32::default(),
                code_address,
                code: ptr::null(),
                code_size: 0,
            };
            let result = unsafe {
                (HOST_INTERFACE.call)((&mut host as *mut HostContext<'_>).cast(), &message)
            };
            assert_eq!(result.status_code, EVMC_FAILURE);
            assert_eq!(host.fault(), Some(expected));
            host.revert_frame().unwrap();
        }

        let mut backend = WitnessBackend::default();
        let mut host = HostContext::new(&mut backend, TxContextOwned::default());
        host.begin_frame().unwrap();
        let delegated = EvmcMessage {
            kind: EVMC_CALL,
            flags: crate::ffi::EVMC_DELEGATED,
            depth: 1,
            gas: 1,
            recipient: EvmcAddress::default(),
            sender: EvmcAddress::default(),
            input_data: ptr::null(),
            input_size: 0,
            value: EvmcBytes32::default(),
            create2_salt: EvmcBytes32::default(),
            code_address: EvmcAddress { bytes: [1; 20] },
            code: ptr::null(),
            code_size: 0,
        };
        let result = unsafe {
            (HOST_INTERFACE.call)((&mut host as *mut HostContext<'_>).cast(), &delegated)
        };
        assert_eq!(result.status_code, EVMC_FAILURE);
        assert_eq!(
            host.fault(),
            Some(HostFault::NestedCallUnsupported { kind: EVMC_CALL })
        );
        host.revert_frame().unwrap();
    }

    #[test]
    fn session_borrow_conflict_is_sticky_instead_of_returning_a_legal_zero() {
        let address = Address([0x11; 20]);
        let mut backend = WitnessBackend::default();
        backend.insert_account(address, Account::new(Word::ZERO, Vec::new()));
        let mut host = HostContext::new(&mut backend, TxContextOwned::default());
        host.begin_frame().unwrap();
        let address_ffi: EvmcAddress = address.into();
        let context = (&mut host as *mut HostContext<'_>).cast();
        let held = host.session.state.borrow_mut();
        let exists = unsafe { (HOST_INTERFACE.account_exists)(context, &address_ffi) };
        drop(held);

        assert!(!exists);
        assert!(matches!(
            host.fault(),
            Some(HostFault::Backend(message))
                if message.contains("account_exists")
        ));
        host.revert_frame().unwrap();
    }

    #[test]
    fn owned_nested_output_uses_an_exact_once_evmc_release() {
        let result = owned_evmc_result(NestedCallResult {
            status_code: EVMC_SUCCESS,
            gas_left: 7,
            gas_refund: 0,
            output: vec![1, 2, 3],
            create_address: Some(Address([0x44; 20])),
        });
        assert_eq!(
            unsafe { slice::from_raw_parts(result.output_data, result.output_size) },
            &[1, 2, 3]
        );
        assert_eq!(result.create_address.bytes, [0x44; 20]);
        let release = result.release.expect("nonempty Rust output owns a release");
        unsafe { release(&result) };

        let empty = owned_evmc_result(NestedCallResult {
            status_code: EVMC_SUCCESS,
            gas_left: 7,
            gas_refund: 0,
            output: Vec::new(),
            create_address: None,
        });
        assert!(empty.output_data.is_null());
        assert!(empty.release.is_none());
    }

    #[test]
    fn fourteen_nonterminal_callbacks_smoke() {
        let address = Address([0x11; 20]);
        let key = w(1);
        let mut backend = WitnessBackend::default();
        backend.insert_account(address, Account::new(w(7), vec![0x60, 0x00]));
        backend.cover_storage(address, key, w(2)).unwrap();
        backend.insert_block_hash(10, w(9));
        let mut host = HostContext::new(&mut backend, TxContextOwned::default());
        host.begin_frame().unwrap();
        let context = (&mut host as *mut HostContext<'_>).cast();
        let address_ffi: EvmcAddress = address.into();
        let key_ffi: EvmcBytes32 = key.into();
        let value_ffi: EvmcBytes32 = w(3).into();
        let topic = EvmcBytes32::from(w(4));
        let data = [5u8, 6];
        let mut code = [0u8; 4];

        unsafe {
            assert!((HOST_INTERFACE.account_exists)(context, &address_ffi));
            assert_eq!(
                Word::from((HOST_INTERFACE.get_storage)(
                    context,
                    &address_ffi,
                    &key_ffi
                )),
                w(2)
            );
            assert_eq!(
                (HOST_INTERFACE.set_storage)(context, &address_ffi, &key_ffi, &value_ffi),
                EVMC_STORAGE_MODIFIED
            );
            assert_eq!(
                Word::from((HOST_INTERFACE.get_balance)(context, &address_ffi)),
                w(7)
            );
            assert_eq!((HOST_INTERFACE.get_code_size)(context, &address_ffi), 2);
            assert_ne!(
                Word::from((HOST_INTERFACE.get_code_hash)(context, &address_ffi)),
                Word::ZERO
            );
            assert_eq!(
                (HOST_INTERFACE.copy_code)(context, &address_ffi, 0, code.as_mut_ptr(), code.len()),
                2
            );
            assert_eq!(&code[..2], &[0x60, 0x00]);
            let _ = (HOST_INTERFACE.get_tx_context)(context);
            assert_eq!(
                Word::from((HOST_INTERFACE.get_block_hash)(context, 10)),
                w(9)
            );
            (HOST_INTERFACE.emit_log)(context, &address_ffi, data.as_ptr(), data.len(), &topic, 1);
            assert_eq!(
                (HOST_INTERFACE.access_account)(context, &address_ffi),
                EVMC_ACCESS_COLD
            );
            assert_eq!(
                (HOST_INTERFACE.access_storage)(context, &address_ffi, &key_ffi),
                EVMC_ACCESS_COLD
            );
            assert_eq!(
                Word::from((HOST_INTERFACE.get_transient_storage)(
                    context,
                    &address_ffi,
                    &key_ffi
                )),
                Word::ZERO
            );
            (HOST_INTERFACE.set_transient_storage)(context, &address_ffi, &key_ffi, &value_ffi);
        }
        assert!(host.fault().is_none());
        assert_eq!(host.audit().len(), 17);
        host.commit_frame().unwrap();
    }

    #[test]
    fn selfdestruct_callback_is_explicitly_unsupported() {
        let address = Address([0x11; 20]);
        let beneficiary = Address([0x22; 20]);
        let mut backend = WitnessBackend::default();
        backend.insert_account(address, Account::new(w(7), Vec::new()));
        backend.insert_account(beneficiary, Account::new(Word::ZERO, Vec::new()));
        let mut host = HostContext::new(&mut backend, TxContextOwned::default());
        host.begin_frame().unwrap();
        let address_ffi: EvmcAddress = address.into();
        let beneficiary_ffi: EvmcAddress = beneficiary.into();
        let result = unsafe {
            (HOST_INTERFACE.selfdestruct)(
                (&mut host as *mut HostContext<'_>).cast(),
                &address_ffi,
                &beneficiary_ffi,
            )
        };
        assert!(!result);
        assert_eq!(host.fault(), Some(HostFault::SelfdestructUnsupported));
        host.revert_frame().unwrap();
    }

    #[test]
    fn storage_coverage_requires_live_account_and_is_cleared_on_replace() {
        let address = Address([0x11; 20]);
        let key = w(1);
        let mut backend = WitnessBackend::default();
        assert_eq!(
            backend.cover_storage(address, key, w(2)),
            Err(HostFault::MissingAccount(address))
        );

        backend.insert_account(address, Account::new(Word::ZERO, Vec::new()));
        backend.cover_storage(address, key, w(2)).unwrap();
        assert_eq!(backend.get_storage(address, key), Ok(w(2)));

        backend.insert_account(address, Account::new(Word::ZERO, Vec::new()));
        assert_eq!(
            backend.get_storage(address, key),
            Err(HostFault::MissingStorage(address, key))
        );

        backend.cover_storage(address, key, w(3)).unwrap();
        backend.cover_absent_account(address);
        assert_eq!(
            backend.get_storage(address, key),
            Err(HostFault::MissingStorage(address, key))
        );
    }

    #[test]
    fn extcode_callback_rejects_backend_code_hash_mismatch() {
        let address = Address([0x11; 20]);
        let mut backend = WitnessBackend::default();
        backend.insert_account(address, Account::new(Word::ZERO, vec![0x60, 0x00]));
        backend.accounts.get_mut(&address).unwrap().code_hash = Word::ZERO;
        let mut host = HostContext::new(&mut backend, TxContextOwned::default());
        host.begin_frame().unwrap();
        let address_ffi: EvmcAddress = address.into();
        let size = unsafe {
            (HOST_INTERFACE.get_code_size)((&mut host as *mut HostContext<'_>).cast(), &address_ffi)
        };
        assert_eq!(size, 0);
        assert_eq!(host.fault(), Some(HostFault::CodeHashMismatch(address)));
        host.revert_frame().unwrap();
    }
}
