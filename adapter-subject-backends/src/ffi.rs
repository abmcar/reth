//! Minimal hand-written EVMC ABI 12 declarations.
//!
//! Layout is pinned to the vendored `evmc.h` at gitlink
//! `88671d2715494436652e49b8e9c73df909d80f38`.

use core::ffi::{c_char, c_int, c_void};

pub const EVMC_ABI_VERSION: i32 = 12;
pub const EVMC_CAPABILITY_EVM1: u32 = 1;
pub const EVMC_CALL: i32 = 0;
pub const EVMC_DELEGATECALL: i32 = 1;
pub const EVMC_CALLCODE: i32 = 2;
pub const EVMC_CREATE: i32 = 3;
pub const EVMC_CREATE2: i32 = 4;
pub const EVMC_EOFCREATE: i32 = 5;
pub const EVMC_STATIC: u32 = 1;
pub const EVMC_DELEGATED: u32 = 2;
pub const EVMC_OSAKA: i32 = 14;

pub const EVMC_SUCCESS: i32 = 0;
pub const EVMC_FAILURE: i32 = 1;
pub const EVMC_REVERT: i32 = 2;
pub const EVMC_OUT_OF_GAS: i32 = 3;
pub const EVMC_INVALID_INSTRUCTION: i32 = 4;
pub const EVMC_UNDEFINED_INSTRUCTION: i32 = 5;
pub const EVMC_STACK_OVERFLOW: i32 = 6;
pub const EVMC_STACK_UNDERFLOW: i32 = 7;
pub const EVMC_BAD_JUMP_DESTINATION: i32 = 8;
pub const EVMC_INVALID_MEMORY_ACCESS: i32 = 9;
pub const EVMC_CALL_DEPTH_EXCEEDED: i32 = 10;
pub const EVMC_STATIC_MODE_VIOLATION: i32 = 11;
pub const EVMC_PRECOMPILE_FAILURE: i32 = 12;
pub const EVMC_CONTRACT_VALIDATION_FAILURE: i32 = 13;
pub const EVMC_ARGUMENT_OUT_OF_RANGE: i32 = 14;
pub const EVMC_INSUFFICIENT_BALANCE: i32 = 17;

pub const EVMC_SET_OPTION_SUCCESS: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EvmcBytes32 {
    pub bytes: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EvmcAddress {
    pub bytes: [u8; 20],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EvmcMessage {
    pub kind: c_int,
    pub flags: u32,
    pub depth: i32,
    pub gas: i64,
    pub recipient: EvmcAddress,
    pub sender: EvmcAddress,
    pub input_data: *const u8,
    pub input_size: usize,
    pub value: EvmcBytes32,
    pub create2_salt: EvmcBytes32,
    pub code_address: EvmcAddress,
    pub code: *const u8,
    pub code_size: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EvmcTxInitcode {
    pub hash: EvmcBytes32,
    pub code: *const u8,
    pub code_size: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EvmcTxContext {
    pub tx_gas_price: EvmcBytes32,
    pub tx_origin: EvmcAddress,
    pub block_coinbase: EvmcAddress,
    pub block_number: i64,
    pub block_timestamp: i64,
    pub block_gas_limit: i64,
    pub block_prev_randao: EvmcBytes32,
    pub chain_id: EvmcBytes32,
    pub block_base_fee: EvmcBytes32,
    pub blob_base_fee: EvmcBytes32,
    pub blob_hashes: *const EvmcBytes32,
    pub blob_hashes_count: usize,
    pub initcodes: *const EvmcTxInitcode,
    pub initcodes_count: usize,
}

impl Default for EvmcTxContext {
    fn default() -> Self {
        Self {
            tx_gas_price: EvmcBytes32::default(),
            tx_origin: EvmcAddress::default(),
            block_coinbase: EvmcAddress::default(),
            block_number: 0,
            block_timestamp: 0,
            block_gas_limit: 0,
            block_prev_randao: EvmcBytes32::default(),
            chain_id: EvmcBytes32::default(),
            block_base_fee: EvmcBytes32::default(),
            blob_base_fee: EvmcBytes32::default(),
            blob_hashes: core::ptr::null(),
            blob_hashes_count: 0,
            initcodes: core::ptr::null(),
            initcodes_count: 0,
        }
    }
}

pub type EvmcReleaseResultFn = unsafe extern "C" fn(result: *const EvmcResult);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EvmcResult {
    pub status_code: c_int,
    pub gas_left: i64,
    pub gas_refund: i64,
    pub output_data: *const u8,
    pub output_size: usize,
    pub release: Option<EvmcReleaseResultFn>,
    pub create_address: EvmcAddress,
    pub padding: [u8; 4],
}

impl EvmcResult {
    pub const fn failure() -> Self {
        Self {
            status_code: EVMC_FAILURE,
            gas_left: 0,
            gas_refund: 0,
            output_data: core::ptr::null(),
            output_size: 0,
            release: None,
            create_address: EvmcAddress { bytes: [0; 20] },
            padding: [0; 4],
        }
    }
}

pub type HostContextOpaque = c_void;

pub type AccountExistsFn = unsafe extern "C" fn(*mut HostContextOpaque, *const EvmcAddress) -> bool;
pub type GetStorageFn = unsafe extern "C" fn(
    *mut HostContextOpaque,
    *const EvmcAddress,
    *const EvmcBytes32,
) -> EvmcBytes32;
pub type SetStorageFn = unsafe extern "C" fn(
    *mut HostContextOpaque,
    *const EvmcAddress,
    *const EvmcBytes32,
    *const EvmcBytes32,
) -> c_int;
pub type GetBalanceFn =
    unsafe extern "C" fn(*mut HostContextOpaque, *const EvmcAddress) -> EvmcBytes32;
pub type GetCodeSizeFn = unsafe extern "C" fn(*mut HostContextOpaque, *const EvmcAddress) -> usize;
pub type GetCodeHashFn =
    unsafe extern "C" fn(*mut HostContextOpaque, *const EvmcAddress) -> EvmcBytes32;
pub type CopyCodeFn = unsafe extern "C" fn(
    *mut HostContextOpaque,
    *const EvmcAddress,
    usize,
    *mut u8,
    usize,
) -> usize;
pub type SelfdestructFn =
    unsafe extern "C" fn(*mut HostContextOpaque, *const EvmcAddress, *const EvmcAddress) -> bool;
pub type CallFn = unsafe extern "C" fn(*mut HostContextOpaque, *const EvmcMessage) -> EvmcResult;
pub type GetTxContextFn = unsafe extern "C" fn(*mut HostContextOpaque) -> EvmcTxContext;
pub type GetBlockHashFn = unsafe extern "C" fn(*mut HostContextOpaque, i64) -> EvmcBytes32;
pub type EmitLogFn = unsafe extern "C" fn(
    *mut HostContextOpaque,
    *const EvmcAddress,
    *const u8,
    usize,
    *const EvmcBytes32,
    usize,
);
pub type AccessAccountFn =
    unsafe extern "C" fn(*mut HostContextOpaque, *const EvmcAddress) -> c_int;
pub type AccessStorageFn =
    unsafe extern "C" fn(*mut HostContextOpaque, *const EvmcAddress, *const EvmcBytes32) -> c_int;
pub type GetTransientStorageFn = GetStorageFn;
pub type SetTransientStorageFn = unsafe extern "C" fn(
    *mut HostContextOpaque,
    *const EvmcAddress,
    *const EvmcBytes32,
    *const EvmcBytes32,
);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EvmcHostInterface {
    pub account_exists: AccountExistsFn,
    pub get_storage: GetStorageFn,
    pub set_storage: SetStorageFn,
    pub get_balance: GetBalanceFn,
    pub get_code_size: GetCodeSizeFn,
    pub get_code_hash: GetCodeHashFn,
    pub copy_code: CopyCodeFn,
    pub selfdestruct: SelfdestructFn,
    pub call: CallFn,
    pub get_tx_context: GetTxContextFn,
    pub get_block_hash: GetBlockHashFn,
    pub emit_log: EmitLogFn,
    pub access_account: AccessAccountFn,
    pub access_storage: AccessStorageFn,
    pub get_transient_storage: GetTransientStorageFn,
    pub set_transient_storage: SetTransientStorageFn,
}

pub type DestroyFn = unsafe extern "C" fn(*mut EvmcVm);
pub type ExecuteFn = unsafe extern "C" fn(
    *mut EvmcVm,
    *const EvmcHostInterface,
    *mut HostContextOpaque,
    c_int,
    *const EvmcMessage,
    *const u8,
    usize,
) -> EvmcResult;
pub type GetCapabilitiesFn = unsafe extern "C" fn(*mut EvmcVm) -> u32;
pub type SetOptionFn = unsafe extern "C" fn(*mut EvmcVm, *const c_char, *const c_char) -> c_int;

#[repr(C)]
pub struct EvmcVm {
    pub abi_version: c_int,
    pub name: *const c_char,
    pub version: *const c_char,
    pub destroy: Option<DestroyFn>,
    pub execute: Option<ExecuteFn>,
    pub get_capabilities: Option<GetCapabilitiesFn>,
    pub set_option: Option<SetOptionFn>,
}

pub type CreateEvmcVmFn = unsafe extern "C" fn() -> *mut EvmcVm;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    /// Expected values come from `tests/abi_probe.c` compiled against the
    /// provenance-pinned vendored EVMC header on x86_64 Linux.
    #[test]
    fn rust_layout_matches_evmc_abi12_c_probe() {
        assert_eq!(size_of::<EvmcAddress>(), 20);
        assert_eq!(size_of::<EvmcBytes32>(), 32);
        assert_eq!(size_of::<EvmcMessage>(), 184);
        assert_eq!(size_of::<EvmcTxContext>(), 256);
        assert_eq!(size_of::<EvmcTxInitcode>(), 48);
        assert_eq!(size_of::<EvmcResult>(), 72);
        assert_eq!(size_of::<EvmcHostInterface>(), 128);
        assert_eq!(size_of::<EvmcVm>(), 56);

        assert_eq!(offset_of!(EvmcMessage, gas), 16);
        assert_eq!(offset_of!(EvmcMessage, input_data), 64);
        assert_eq!(offset_of!(EvmcMessage, value), 80);
        assert_eq!(offset_of!(EvmcMessage, code_address), 144);
        assert_eq!(offset_of!(EvmcMessage, code), 168);
        assert_eq!(offset_of!(EvmcMessage, code_size), 176);
        assert_eq!(offset_of!(EvmcTxContext, blob_hashes), 224);
        assert_eq!(offset_of!(EvmcTxContext, initcodes), 240);
        assert_eq!(offset_of!(EvmcResult, gas_left), 8);
        assert_eq!(offset_of!(EvmcResult, gas_refund), 16);
        assert_eq!(offset_of!(EvmcResult, release), 40);
        assert_eq!(offset_of!(EvmcResult, create_address), 48);
        assert_eq!(offset_of!(EvmcHostInterface, set_transient_storage), 120);
        assert_eq!(offset_of!(EvmcVm, execute), 32);
        assert_eq!(offset_of!(EvmcVm, set_option), 48);
    }
}
