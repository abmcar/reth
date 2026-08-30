//! Allowlisted EVMC subject loader and one-frame execution gate.

use crate::{
    ffi::{
        CreateEvmcVmFn, EVMC_ABI_VERSION, EVMC_ARGUMENT_OUT_OF_RANGE, EVMC_BAD_JUMP_DESTINATION,
        EVMC_CALL, EVMC_CALL_DEPTH_EXCEEDED, EVMC_CAPABILITY_EVM1,
        EVMC_CONTRACT_VALIDATION_FAILURE, EVMC_CREATE, EVMC_CREATE2, EVMC_DELEGATED,
        EVMC_EOFCREATE, EVMC_INSUFFICIENT_BALANCE, EVMC_INVALID_INSTRUCTION,
        EVMC_INVALID_MEMORY_ACCESS, EVMC_OSAKA, EVMC_OUT_OF_GAS, EVMC_PRECOMPILE_FAILURE,
        EVMC_REVERT, EVMC_SET_OPTION_SUCCESS, EVMC_STACK_OVERFLOW, EVMC_STACK_UNDERFLOW,
        EVMC_STATIC_MODE_VIOLATION, EVMC_SUCCESS, EVMC_UNDEFINED_INSTRUCTION, EvmcBytes32,
        EvmcMessage, EvmcResult, EvmcVm,
    },
    host::{AccessEvent, Address, HOST_INTERFACE, HostContext, HostFault, ReentrantVm, Word},
};
use libloading::Library;
use std::{
    ffi::{CStr, CString},
    marker::PhantomData,
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    rc::Rc,
    slice,
    sync::{Arc, Mutex},
};
use thiserror::Error;

pub const SUBJECT_BACKEND_ENV: &str = "RETH_SUBJECT_BACKEND";
/// Optional comma-separated extra EVMC options, e.g.
/// `code_cache_dir=/path,code_cache_mode=rw`. Applied after the mandatory
/// options; a rejected or malformed entry fails startup loudly.
pub const SUBJECT_EXTRA_OPTIONS_ENV: &str = "RETH_SUBJECT_EVMC_OPTIONS";

const DTVM_PROFILE_GUIDED_OPTIONS: &[(&str, &str)] = &[
    ("mode", "multipass"),
    ("enable_gas_metering", "true"),
    ("profile_guided_jit", "true"),
];
const DTVM_EAGER_OPTIONS: &[(&str, &str)] = &[
    ("mode", "multipass"),
    ("enable_gas_metering", "true"),
    ("profile_guided_jit", "false"),
];
const EVMONE_ADVANCED_OPTIONS: &[(&str, &str)] = &[("advanced", "")];
// evmone's default execution path is `baseline::execute`; `advanced` is an
// opt-in selected through set_option. The baseline leg sets no option at all,
// so it measures the mode an unconfigured evmone actually runs.
const EVMONE_BASELINE_OPTIONS: &[(&str, &str)] = &[];

const DTVM_EVMC_PHASE_METRICS_VERSION: u32 = 2;
const DTVM_EVMC_PHASE_METRICS_STRUCT_SIZE: u32 = 192;
const DTVM_EVMC_HOT_METRICS_VERSION: u32 = 2;
const DTVM_EVMC_HOT_METRICS_STRUCT_SIZE: u32 = 192;
const EVMONE_ADVANCED_DIAGNOSTIC_METRICS_VERSION: u32 = 1;
const EVMONE_ADVANCED_DIAGNOSTIC_METRICS_STRUCT_SIZE: u32 = 72;
const DTVM_GET_EVMC_PHASE_METRICS_SYMBOL: &[u8] = b"dtvm_get_evmc_phase_metrics\0";
const DTVM_RESET_EVMC_PHASE_METRICS_SYMBOL: &[u8] = b"dtvm_reset_evmc_phase_metrics\0";
const EVMONE_GET_ADVANCED_DIAGNOSTIC_METRICS_SYMBOL: &[u8] =
    b"evmone_get_advanced_diagnostic_metrics\0";

type GetDtvmEvmcPhaseMetricsFn =
    unsafe extern "C" fn(*mut EvmcVm, *mut DtvmEvmcPhaseMetrics) -> i32;
type ResetDtvmEvmcPhaseMetricsFn = unsafe extern "C" fn(*mut EvmcVm, u32, u32) -> i32;
type GetDtvmEvmcHotMetricsFn = unsafe extern "C" fn(*mut EvmcVm, *mut DtvmEvmcHotMetrics) -> i32;
type GetEvmoneAdvancedDiagnosticMetricsFn =
    unsafe extern "C" fn(*mut EvmcVm, *mut EvmoneAdvancedDiagnosticMetrics) -> i32;

/// Required monotonic DTVM counters for fail-closed hot-cache replay.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DtvmEvmcHotMetrics {
    pub version: u32,
    pub struct_size: u32,
    pub top_level_execute_count: u64,
    pub top_level_execute_wall_ns: u64,
    pub synchronous_jit_compile_attempt_count: u64,
    pub synchronous_jit_compile_success_count: u64,
    pub synchronous_jit_compile_wall_ns: u64,
    pub non_compile_residual_ns: u64,
    pub profile_guided_jit_trigger_count: u64,
    pub module_cache_lookup_count: u64,
    pub module_cache_hit_count: u64,
    pub module_cache_miss_count: u64,
    pub module_cache_validation_reject_count: u64,
    pub module_cache_eviction_count: u64,
    pub module_cache_entry_count: u64,
    pub module_cache_peak_entry_count: u64,
    pub transient_module_load_count: u64,
    pub jit_frame_count: u64,
    pub jit_active_wall_ns: u64,
    pub interpreter_frame_count: u64,
    pub interpreter_active_wall_ns: u64,
    pub create_interpreter_fallback_count: u64,
    pub newly_created_interpreter_fallback_count: u64,
    pub small_code_interpreter_fallback_count: u64,
    pub sticky_interpreter_fallback_count: u64,
}

impl DtvmEvmcHotMetrics {
    fn request() -> Self {
        Self {
            version: DTVM_EVMC_HOT_METRICS_VERSION,
            struct_size: DTVM_EVMC_HOT_METRICS_STRUCT_SIZE,
            ..Self::default()
        }
    }

    fn has_expected_layout(self) -> bool {
        self.version == DTVM_EVMC_HOT_METRICS_VERSION
            && self.struct_size == DTVM_EVMC_HOT_METRICS_STRUCT_SIZE
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        macro_rules! sum {
            ($field:ident) => {
                self.$field.checked_add(other.$field)?
            };
        }
        Some(Self {
            version: DTVM_EVMC_HOT_METRICS_VERSION,
            struct_size: DTVM_EVMC_HOT_METRICS_STRUCT_SIZE,
            top_level_execute_count: sum!(top_level_execute_count),
            top_level_execute_wall_ns: sum!(top_level_execute_wall_ns),
            synchronous_jit_compile_attempt_count: sum!(synchronous_jit_compile_attempt_count),
            synchronous_jit_compile_success_count: sum!(synchronous_jit_compile_success_count),
            synchronous_jit_compile_wall_ns: sum!(synchronous_jit_compile_wall_ns),
            non_compile_residual_ns: sum!(non_compile_residual_ns),
            profile_guided_jit_trigger_count: sum!(profile_guided_jit_trigger_count),
            module_cache_lookup_count: sum!(module_cache_lookup_count),
            module_cache_hit_count: sum!(module_cache_hit_count),
            module_cache_miss_count: sum!(module_cache_miss_count),
            module_cache_validation_reject_count: sum!(module_cache_validation_reject_count),
            module_cache_eviction_count: sum!(module_cache_eviction_count),
            module_cache_entry_count: sum!(module_cache_entry_count),
            module_cache_peak_entry_count: sum!(module_cache_peak_entry_count),
            transient_module_load_count: sum!(transient_module_load_count),
            jit_frame_count: sum!(jit_frame_count),
            jit_active_wall_ns: sum!(jit_active_wall_ns),
            interpreter_frame_count: sum!(interpreter_frame_count),
            interpreter_active_wall_ns: sum!(interpreter_active_wall_ns),
            create_interpreter_fallback_count: sum!(create_interpreter_fallback_count),
            newly_created_interpreter_fallback_count: sum!(
                newly_created_interpreter_fallback_count
            ),
            small_code_interpreter_fallback_count: sum!(small_code_interpreter_fallback_count),
            sticky_interpreter_fallback_count: sum!(sticky_interpreter_fallback_count),
        })
    }
}

const _: () = assert!(core::mem::size_of::<DtvmEvmcHotMetrics>() == 192);

/// The single-block collector and hot-cache worker share one ABI v2 layout.
pub type DtvmEvmcPhaseMetrics = DtvmEvmcHotMetrics;

/// Read-only metrics from the instrumented evmone advanced C ABI path.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EvmoneAdvancedDiagnosticMetrics {
    pub version: u32,
    pub struct_size: u32,
    pub top_level_execute_count: u64,
    pub top_level_execute_wall_ns: u64,
    pub advanced_analysis_count: u64,
    pub advanced_analysis_wall_ns: u64,
    pub advanced_state_setup_count: u64,
    pub advanced_state_setup_wall_ns: u64,
    pub advanced_core_execute_count: u64,
    pub advanced_core_execute_wall_ns: u64,
}

impl EvmoneAdvancedDiagnosticMetrics {
    fn request() -> Self {
        Self {
            version: EVMONE_ADVANCED_DIAGNOSTIC_METRICS_VERSION,
            struct_size: EVMONE_ADVANCED_DIAGNOSTIC_METRICS_STRUCT_SIZE,
            ..Self::default()
        }
    }

    fn has_expected_layout(self) -> bool {
        self.version == EVMONE_ADVANCED_DIAGNOSTIC_METRICS_VERSION
            && self.struct_size == EVMONE_ADVANCED_DIAGNOSTIC_METRICS_STRUCT_SIZE
    }

    fn counters_are_zero(self) -> bool {
        self.top_level_execute_count == 0
            && self.top_level_execute_wall_ns == 0
            && self.advanced_analysis_count == 0
            && self.advanced_analysis_wall_ns == 0
            && self.advanced_state_setup_count == 0
            && self.advanced_state_setup_wall_ns == 0
            && self.advanced_core_execute_count == 0
            && self.advanced_core_execute_wall_ns == 0
    }
}

const _: () = assert!(core::mem::size_of::<EvmoneAdvancedDiagnosticMetrics>() == 72);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtvmPhaseMetricsStatus {
    Success,
    InvalidArgument,
    Disabled,
    Busy,
    Incompatible,
    Inconsistent,
    Unknown(i32),
}

impl DtvmPhaseMetricsStatus {
    pub const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::InvalidArgument => 1,
            Self::Disabled => 2,
            Self::Busy => 3,
            Self::Incompatible => 4,
            Self::Inconsistent => 5,
            Self::Unknown(code) => code,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::InvalidArgument => "invalid-argument",
            Self::Disabled => "disabled",
            Self::Busy => "busy",
            Self::Incompatible => "incompatible",
            Self::Inconsistent => "inconsistent",
            Self::Unknown(_) => "unknown",
        }
    }

    const fn from_code(code: i32) -> Self {
        match code {
            0 => Self::Success,
            1 => Self::InvalidArgument,
            2 => Self::Disabled,
            3 => Self::Busy,
            4 => Self::Incompatible,
            5 => Self::Inconsistent,
            code => Self::Unknown(code),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtvmPhaseMetricsReport {
    pub status: DtvmPhaseMetricsStatus,
    pub metrics: Option<DtvmEvmcPhaseMetrics>,
}

#[derive(Debug, Default)]
struct DtvmPhaseMetricsState {
    status: Option<DtvmPhaseMetricsStatus>,
    metrics: Option<DtvmEvmcPhaseMetrics>,
}

#[derive(Clone, Debug, Default)]
pub struct DtvmPhaseMetricsCollector {
    state: Arc<Mutex<DtvmPhaseMetricsState>>,
}

impl DtvmPhaseMetricsCollector {
    pub fn report(&self) -> DtvmPhaseMetricsReport {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        DtvmPhaseMetricsReport {
            status: state.status.unwrap_or(DtvmPhaseMetricsStatus::Disabled),
            metrics: state.metrics,
        }
    }

    fn record(&self, status: DtvmPhaseMetricsStatus, metrics: Option<DtvmEvmcPhaseMetrics>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if status != DtvmPhaseMetricsStatus::Success {
            if state.status.is_some_and(|current| current != status) {
                state.status = Some(DtvmPhaseMetricsStatus::Inconsistent);
            } else {
                state.status = Some(status);
            }
            state.metrics = None;
            return;
        }

        let Some(metrics) = metrics.filter(|metrics| metrics.has_expected_layout()) else {
            state.status = Some(DtvmPhaseMetricsStatus::Inconsistent);
            state.metrics = None;
            return;
        };
        if state.status.is_some_and(|current| current != status) {
            state.status = Some(DtvmPhaseMetricsStatus::Inconsistent);
            state.metrics = None;
            return;
        }
        state.status = Some(status);
        state.metrics = match state.metrics {
            Some(accumulated) => accumulated.checked_add(metrics),
            None => Some(metrics),
        };
        if state.metrics.is_none() {
            state.status = Some(DtvmPhaseMetricsStatus::Inconsistent);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectBackend {
    DtvmEager,
    DtvmProfileGuided,
    EvmoneAdvanced,
    EvmoneBaseline,
}

impl SubjectBackend {
    pub fn from_env() -> Result<Self, DtvmError> {
        match std::env::var(SUBJECT_BACKEND_ENV) {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Err(DtvmError::MissingSubjectBackend),
            Err(std::env::VarError::NotUnicode(value)) => Err(DtvmError::InvalidSubjectBackend(
                value.to_string_lossy().into_owned(),
            )),
        }
    }

    pub fn parse(value: &str) -> Result<Self, DtvmError> {
        match value {
            "dtvm-eager" => Ok(Self::DtvmEager),
            "dtvm-profile-guided" => Ok(Self::DtvmProfileGuided),
            "evmone-advanced" => Ok(Self::EvmoneAdvanced),
            "evmone-baseline" => Ok(Self::EvmoneBaseline),
            value => Err(DtvmError::InvalidSubjectBackend(value.to_owned())),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DtvmEager => "dtvm-eager",
            Self::DtvmProfileGuided => "dtvm-profile-guided",
            Self::EvmoneAdvanced => "evmone-advanced",
            Self::EvmoneBaseline => "evmone-baseline",
        }
    }

    const fn factory_symbol(self) -> &'static [u8] {
        match self {
            Self::DtvmEager | Self::DtvmProfileGuided => b"evmc_create_dtvmapi\0",
            Self::EvmoneAdvanced | Self::EvmoneBaseline => b"evmc_create_evmone\0",
        }
    }

    const fn factory_symbol_name(self) -> &'static str {
        match self {
            Self::DtvmEager | Self::DtvmProfileGuided => "evmc_create_dtvmapi",
            Self::EvmoneAdvanced | Self::EvmoneBaseline => "evmc_create_evmone",
        }
    }

    const fn expected_vm_name(self) -> &'static str {
        match self {
            Self::DtvmEager | Self::DtvmProfileGuided => "dtvm",
            Self::EvmoneAdvanced | Self::EvmoneBaseline => "evmone",
        }
    }

    const fn mandatory_options(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::DtvmEager => DTVM_EAGER_OPTIONS,
            Self::DtvmProfileGuided => DTVM_PROFILE_GUIDED_OPTIONS,
            Self::EvmoneAdvanced => EVMONE_ADVANCED_OPTIONS,
            Self::EvmoneBaseline => EVMONE_BASELINE_OPTIONS,
        }
    }

    const fn requires_strict_address_cache(self) -> bool {
        matches!(self, Self::DtvmEager | Self::DtvmProfileGuided)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameStatus {
    Success,
    Revert,
    Halt(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameOutcome {
    pub status: FrameStatus,
    pub gas_left: u64,
    /// Frame-local refund may be negative before Reth applies transaction-level
    /// refund accounting and caps it.
    pub gas_refund: i64,
    pub output: Vec<u8>,
    pub audit: Vec<AccessEvent>,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub kind: i32,
    pub recipient: Address,
    pub sender: Address,
    pub code_address: Address,
    pub input: Vec<u8>,
    pub value: Word,
    pub gas: u64,
    pub flags: u32,
}

impl Message {
    pub fn ordinary(
        recipient: Address,
        sender: Address,
        input: Vec<u8>,
        value: Word,
        gas: u64,
    ) -> Self {
        Self {
            kind: EVMC_CALL,
            recipient,
            sender,
            code_address: recipient,
            input,
            value,
            gas,
            flags: 0,
        }
    }

    pub fn create(recipient: Address, sender: Address, value: Word, gas: u64) -> Self {
        Self {
            kind: EVMC_CREATE,
            recipient,
            sender,
            code_address: Address::default(),
            input: Vec::new(),
            value,
            gas,
            flags: 0,
        }
    }
}

#[derive(Debug, Error)]
pub enum DtvmError {
    #[error(
        "strict address-cache validation is not hard-enabled; export \
         DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true before loading DTVM"
    )]
    StrictCacheNotEnabled,
    #[error(
        "{SUBJECT_BACKEND_ENV} is required and must equal dtvm-eager, dtvm-profile-guided, or \
         evmone-advanced"
    )]
    MissingSubjectBackend,
    #[error(
        "{SUBJECT_BACKEND_ENV} must equal dtvm-eager, dtvm-profile-guided, or evmone-advanced, got \
         {0:?}"
    )]
    InvalidSubjectBackend(String),
    #[error("failed to load {backend} EVMC library {path}: {reason}")]
    LibraryLoad {
        backend: &'static str,
        path: PathBuf,
        reason: String,
    },
    #[error("{backend} factory symbol {symbol} is missing: {reason}")]
    MissingCreateSymbol {
        backend: &'static str,
        symbol: &'static str,
        reason: String,
    },
    #[error("{backend} factory symbol {symbol} returned null")]
    NullVm {
        backend: &'static str,
        symbol: &'static str,
    },
    #[error("EVMC subject advertises ABI {actual}, expected {expected}")]
    AbiMismatch { actual: i32, expected: i32 },
    #[error("EVMC subject vtable is incomplete: {0}")]
    IncompleteVtable(&'static str),
    #[error("EVMC subject metadata is invalid: {0}")]
    InvalidMetadata(&'static str),
    #[error("EVMC subject name mismatch: expected {expected:?}, got {actual:?}")]
    UnexpectedVmName {
        expected: &'static str,
        actual: String,
    },
    #[error("EVMC subject lacks EVMC_CAPABILITY_EVM1 (capabilities=0x{0:x})")]
    MissingEvm1Capability(u32),
    #[error("EVMC subject rejected mandatory option {name}={value:?} (result={result})")]
    OptionRejected {
        name: &'static str,
        value: &'static str,
        result: i32,
    },
    #[error("EVMC subject rejected extra option {name}={value:?} (result={result})")]
    ExtraOptionRejected {
        name: String,
        value: String,
        result: i32,
    },
    #[error("malformed {SUBJECT_EXTRA_OPTIONS_ENV} entry {entry:?}: expected name=value")]
    ExtraOptionMalformed { entry: String },
    #[error("required DTVM hot-cache metrics ABI v2 is unavailable: {0}")]
    RequiredHotMetrics(String),
    #[error("required evmone advanced diagnostic metrics ABI v1 is unavailable: {0}")]
    RequiredEvmoneDiagnosticMetrics(String),
    #[error("only EVMC Osaka revision 14 is accepted, got {0}")]
    UnsupportedRevision(i32),
    #[error("unsupported depth-0 EVMC message: {0}")]
    UnsupportedMessage(&'static str),
    #[error("gas exceeds EVMC int64 range: {0}")]
    GasOutOfRange(u64),
    #[error("host checkpoint failed: {0}")]
    Checkpoint(HostFault),
    #[error("host checkpoint commit failed ({commit}); rollback also failed ({rollback})")]
    CommitRollback {
        commit: HostFault,
        rollback: HostFault,
    },
    #[error("host callback failed closed: {0}")]
    Host(HostFault),
    #[error("EVMC subject returned backend/internal status {0}")]
    BackendStatus(i32),
    #[error("EVMC subject result invariant failed: {0}")]
    ResultInvariant(&'static str),
}

/// Loaded allowlisted EVMC subject.
///
/// The `Rc` marker deliberately prevents cross-thread transfer: the current
/// DTVM instance owns mutable caches and is used synchronously/reentrantly.
pub struct Dtvm {
    vm: NonNull<EvmcVm>,
    library: Library,
    library_path: PathBuf,
    backend: SubjectBackend,
    phase_metrics: Option<DtvmPhaseMetricsInstance>,
    hot_metrics: Option<DtvmHotMetricsInstance>,
    evmone_diagnostic_metrics: Option<EvmoneAdvancedDiagnosticMetricsInstance>,
    name: String,
    version: String,
    _not_send_sync: PhantomData<Rc<()>>,
}

impl core::fmt::Debug for Dtvm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dtvm")
            .field("library_path", &self.library_path)
            .field("backend", &self.backend)
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Dtvm {
    /// Loads the environment-selected EVMC subject and enforces its gates.
    ///
    /// # Safety
    ///
    /// `path` must name the trusted EVMC build whose provenance is recorded by
    /// the experiment. Dynamic library constructors execute arbitrary code.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, DtvmError> {
        let backend = SubjectBackend::from_env()?;
        // SAFETY: forwarded caller contract and fixed allowlisted policy.
        unsafe { Self::load_for(path, backend) }
    }

    /// Loads one explicitly selected allowlisted subject.
    ///
    /// # Safety
    ///
    /// `path` must name the provenance-pinned library for `backend`.
    pub unsafe fn load_for(
        path: impl AsRef<Path>,
        backend: SubjectBackend,
    ) -> Result<Self, DtvmError> {
        // SAFETY: forwarded caller contract and fixed allowlisted policy.
        unsafe { Self::load_for_inner(path, backend, None) }
    }

    /// Loads one DTVM eager subject and aggregates per-VM phase snapshots.
    ///
    /// # Safety
    ///
    /// `path` must name the provenance-pinned DTVM shared object.
    pub unsafe fn load_for_with_phase_metrics(
        path: impl AsRef<Path>,
        backend: SubjectBackend,
        collector: DtvmPhaseMetricsCollector,
    ) -> Result<Self, DtvmError> {
        // SAFETY: forwarded caller contract and fixed allowlisted policy.
        unsafe { Self::load_for_inner(path, backend, Some(collector)) }
    }

    unsafe fn load_for_inner(
        path: impl AsRef<Path>,
        backend: SubjectBackend,
        phase_metrics_collector: Option<DtvmPhaseMetricsCollector>,
    ) -> Result<Self, DtvmError> {
        if backend.requires_strict_address_cache()
            && std::env::var("DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION").as_deref() != Ok("true")
        {
            return Err(DtvmError::StrictCacheNotEnabled);
        }

        let path = path.as_ref().to_path_buf();
        // SAFETY: caller designates a provenance-pinned EVMC shared object.
        let library = unsafe { Library::new(&path) }.map_err(|error| DtvmError::LibraryLoad {
            backend: backend.as_str(),
            path: path.clone(),
            reason: error.to_string(),
        })?;
        // SAFETY: both allowlisted symbols have the EVMC factory signature.
        let create = unsafe { library.get::<CreateEvmcVmFn>(backend.factory_symbol()) }.map_err(
            |error| DtvmError::MissingCreateSymbol {
                backend: backend.as_str(),
                symbol: backend.factory_symbol_name(),
                reason: error.to_string(),
            },
        )?;
        // SAFETY: EVMC factory owns and returns a new VM instance.
        let vm = NonNull::new(unsafe { create() }).ok_or(DtvmError::NullVm {
            backend: backend.as_str(),
            symbol: backend.factory_symbol_name(),
        })?;
        // Drop the symbol borrow before moving `library`.
        drop(create);

        // SAFETY: non-null EVMC VM points to at least the ABI vtable.
        let raw = unsafe { vm.as_ref() };
        if raw.abi_version != EVMC_ABI_VERSION {
            destroy_if_present(vm);
            return Err(DtvmError::AbiMismatch {
                actual: raw.abi_version,
                expected: EVMC_ABI_VERSION,
            });
        }
        if raw.destroy.is_none() {
            return Err(destroy_with_error(
                vm,
                DtvmError::IncompleteVtable("destroy"),
            ));
        }
        if raw.execute.is_none() {
            return Err(destroy_with_error(
                vm,
                DtvmError::IncompleteVtable("execute"),
            ));
        }
        let get_capabilities = raw.get_capabilities.ok_or_else(|| {
            destroy_with_error(vm, DtvmError::IncompleteVtable("get_capabilities"))
        })?;
        let set_option = raw
            .set_option
            .ok_or_else(|| destroy_with_error(vm, DtvmError::IncompleteVtable("set_option")))?;
        let name = c_metadata(raw.name, "name").map_err(|error| destroy_with_error(vm, error))?;
        let version =
            c_metadata(raw.version, "version").map_err(|error| destroy_with_error(vm, error))?;
        if name != backend.expected_vm_name() {
            return Err(destroy_with_error(
                vm,
                DtvmError::UnexpectedVmName {
                    expected: backend.expected_vm_name(),
                    actual: name,
                },
            ));
        }

        set_mandatory_options(vm, set_option, backend)?;
        set_extra_options_from_env(vm, set_option)?;
        // EVMC requires clients to query capabilities after all options have
        // been applied because options are allowed to change this value.
        // SAFETY: mandatory vtable function and live VM.
        let capabilities = unsafe { get_capabilities(vm.as_ptr()) };
        if capabilities & EVMC_CAPABILITY_EVM1 == 0 {
            return Err(destroy_with_error(
                vm,
                DtvmError::MissingEvm1Capability(capabilities),
            ));
        }

        let phase_metrics = if backend == SubjectBackend::DtvmEager {
            phase_metrics_collector
                .and_then(|collector| initialize_phase_metrics(&library, vm, collector))
        } else {
            None
        };

        Ok(Self {
            vm,
            library,
            library_path: path,
            backend,
            phase_metrics,
            hot_metrics: None,
            evmone_diagnostic_metrics: None,
            name,
            version,
            _not_send_sync: PhantomData,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn backend(&self) -> SubjectBackend {
        self.backend
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn library_path(&self) -> &Path {
        &self.library_path
    }

    pub fn library_is_loaded(&self) -> bool {
        let _keepalive = &self.library;
        true
    }

    /// Switches this quiescent eager VM to the required monotonic hot-cache ABI.
    pub fn require_hot_metrics_v2(&mut self) -> Result<(), DtvmError> {
        if self.backend != SubjectBackend::DtvmEager {
            return Err(DtvmError::RequiredHotMetrics(format!(
                "backend {} is not dtvm-eager",
                self.backend.as_str()
            )));
        }
        // SAFETY: the exact signatures and POD layout are the versioned ABI
        // contract. Function pointers remain valid while `self.library` lives.
        let reset = unsafe {
            self.library
                .get::<ResetDtvmEvmcPhaseMetricsFn>(DTVM_RESET_EVMC_PHASE_METRICS_SYMBOL)
                .map(|symbol| *symbol)
        }
        .map_err(|error| DtvmError::RequiredHotMetrics(format!("reset symbol: {error}")))?;
        // SAFETY: same versioned diagnostic ABI contract as above.
        let get = unsafe {
            self.library
                .get::<GetDtvmEvmcHotMetricsFn>(DTVM_GET_EVMC_PHASE_METRICS_SYMBOL)
                .map(|symbol| *symbol)
        }
        .map_err(|error| DtvmError::RequiredHotMetrics(format!("get symbol: {error}")))?;
        // SAFETY: batch setup calls this before the first execute.
        let status = DtvmPhaseMetricsStatus::from_code(unsafe {
            reset(
                self.vm.as_ptr(),
                DTVM_EVMC_HOT_METRICS_VERSION,
                DTVM_EVMC_HOT_METRICS_STRUCT_SIZE,
            )
        });
        if status != DtvmPhaseMetricsStatus::Success {
            return Err(DtvmError::RequiredHotMetrics(format!(
                "reset returned {} ({})",
                status.as_str(),
                status.code()
            )));
        }
        self.phase_metrics = None;
        self.hot_metrics = Some(DtvmHotMetricsInstance { get });
        self.hot_metrics_snapshot()?;
        Ok(())
    }

    /// Returns one monotonic v2 snapshot without resetting the VM or its cache.
    pub fn hot_metrics_snapshot(&self) -> Result<DtvmEvmcHotMetrics, DtvmError> {
        let interface = self.hot_metrics.as_ref().ok_or_else(|| {
            DtvmError::RequiredHotMetrics("v2 interface was not initialized".to_string())
        })?;
        let mut metrics = DtvmEvmcHotMetrics::request();
        // SAFETY: the VM and output live for the complete synchronous call.
        let status = DtvmPhaseMetricsStatus::from_code(unsafe {
            (interface.get)(self.vm.as_ptr(), &mut metrics)
        });
        if status != DtvmPhaseMetricsStatus::Success {
            return Err(DtvmError::RequiredHotMetrics(format!(
                "snapshot returned {} ({})",
                status.as_str(),
                status.code()
            )));
        }
        if !metrics.has_expected_layout() {
            return Err(DtvmError::RequiredHotMetrics(format!(
                "snapshot layout is version {} size {}, expected version {} size {}",
                metrics.version,
                metrics.struct_size,
                DTVM_EVMC_HOT_METRICS_VERSION,
                DTVM_EVMC_HOT_METRICS_STRUCT_SIZE
            )));
        }
        Ok(metrics)
    }

    /// Requires the read-only diagnostic ABI on a fresh evmone advanced VM.
    pub fn require_evmone_diagnostic_metrics_v1(&mut self) -> Result<(), DtvmError> {
        if self.backend != SubjectBackend::EvmoneAdvanced {
            return Err(DtvmError::RequiredEvmoneDiagnosticMetrics(format!(
                "backend {} is not evmone-advanced",
                self.backend.as_str()
            )));
        }
        // SAFETY: the symbol signature and 72-byte POD layout are versioned by
        // the diagnostic evmone build and remain valid while the library lives.
        let get = unsafe {
            self.library
                .get::<GetEvmoneAdvancedDiagnosticMetricsFn>(
                    EVMONE_GET_ADVANCED_DIAGNOSTIC_METRICS_SYMBOL,
                )
                .map(|symbol| *symbol)
        }
        .map_err(|error| {
            DtvmError::RequiredEvmoneDiagnosticMetrics(format!("get symbol: {error}"))
        })?;
        self.evmone_diagnostic_metrics = Some(EvmoneAdvancedDiagnosticMetricsInstance { get });
        let initial = self.evmone_diagnostic_metrics_snapshot()?;
        if !initial.counters_are_zero() {
            self.evmone_diagnostic_metrics = None;
            return Err(DtvmError::RequiredEvmoneDiagnosticMetrics(
                "fresh VM returned nonzero counters".to_string(),
            ));
        }
        Ok(())
    }

    /// Returns one quiescent monotonic snapshot without resetting the VM.
    pub fn evmone_diagnostic_metrics_snapshot(
        &self,
    ) -> Result<EvmoneAdvancedDiagnosticMetrics, DtvmError> {
        let interface = self.evmone_diagnostic_metrics.as_ref().ok_or_else(|| {
            DtvmError::RequiredEvmoneDiagnosticMetrics(
                "v1 interface was not initialized".to_string(),
            )
        })?;
        let mut metrics = EvmoneAdvancedDiagnosticMetrics::request();
        // SAFETY: the VM and output live for the complete synchronous call.
        let status = unsafe { (interface.get)(self.vm.as_ptr(), &mut metrics) };
        if status != 0 {
            return Err(DtvmError::RequiredEvmoneDiagnosticMetrics(format!(
                "snapshot returned {status}"
            )));
        }
        if !metrics.has_expected_layout() {
            return Err(DtvmError::RequiredEvmoneDiagnosticMetrics(format!(
                "snapshot layout is version {} size {}, expected version {} size {}",
                metrics.version,
                metrics.struct_size,
                EVMONE_ADVANCED_DIAGNOSTIC_METRICS_VERSION,
                EVMONE_ADVANCED_DIAGNOSTIC_METRICS_STRUCT_SIZE
            )));
        }
        Ok(metrics)
    }

    /// Returns the current optional v2 snapshot without destroying the VM.
    pub fn phase_metrics_snapshot(&self) -> Option<DtvmPhaseMetricsReport> {
        let interface = self.phase_metrics.as_ref()?;
        let mut metrics = DtvmEvmcPhaseMetrics::request();
        // SAFETY: the VM and output live for the complete synchronous call.
        let status = DtvmPhaseMetricsStatus::from_code(unsafe {
            (interface.get)(self.vm.as_ptr(), &mut metrics)
        });
        Some(DtvmPhaseMetricsReport {
            status,
            metrics: (status == DtvmPhaseMetricsStatus::Success && metrics.has_expected_layout())
                .then_some(metrics),
        })
    }

    /// Executes one ordinary Osaka call or CREATE initcode frame.
    pub fn execute(
        &mut self,
        revision: i32,
        message: &Message,
        code: &[u8],
        host: &mut HostContext<'_>,
    ) -> Result<FrameOutcome, DtvmError> {
        validate_request(revision, message)?;
        let gas = i64::try_from(message.gas).map_err(|_| DtvmError::GasOutOfRange(message.gas))?;
        // SAFETY: the loader already validated the vtable and the dynamic
        // library remains owned by `self` for this complete synchronous call.
        let execute = unsafe { self.vm.as_ref() }
            .execute
            .ok_or(DtvmError::IncompleteVtable("execute"))?;
        host.begin_frame().map_err(DtvmError::Checkpoint)?;
        host.install_reentrant_vm(ReentrantVm {
            vm: self.vm,
            execute,
            revision,
        });

        let execution = (|| {
            match message.kind {
                EVMC_CALL => host
                    .validate_top_level(message.recipient, message.code_address, code)
                    .map_err(DtvmError::Host)?,
                EVMC_CREATE => host
                    .validate_top_level_create(message.recipient)
                    .map_err(DtvmError::Host)?,
                _ => {
                    return Err(DtvmError::UnsupportedMessage(
                        "message kind passed validation but is not executable",
                    ));
                }
            }

            let raw_message = EvmcMessage {
                kind: message.kind,
                flags: message.flags,
                depth: 0,
                gas,
                recipient: message.recipient.into(),
                sender: message.sender.into(),
                input_data: null_if_empty(&message.input),
                input_size: message.input.len(),
                value: message.value.into(),
                create2_salt: EvmcBytes32::default(),
                code_address: message.code_address.into(),
                code: null_if_empty(code),
                code_size: code.len(),
            };
            let result = unsafe {
                execute(
                    self.vm.as_ptr(),
                    &HOST_INTERFACE,
                    (host as *mut HostContext<'_>).cast(),
                    revision,
                    &raw_message,
                    null_if_empty(code),
                    code.len(),
                )
            };
            let result = ResultGuard(result);

            let output = copy_output(&result.0)?;
            validate_result(&result.0, gas)?;
            if let Some(fault) = host.take_fault() {
                return Err(DtvmError::Host(fault));
            }

            let status = match result.0.status_code {
                EVMC_SUCCESS => FrameStatus::Success,
                EVMC_REVERT => FrameStatus::Revert,
                status if is_consensus_halt(status) => FrameStatus::Halt(status),
                status => return Err(DtvmError::BackendStatus(status)),
            };
            Ok(FrameOutcome {
                status,
                gas_left: result.0.gas_left as u64,
                gas_refund: result.0.gas_refund,
                output,
                audit: host.audit(),
            })
        })();
        host.clear_reentrant_vm();

        match execution {
            Ok(outcome) if matches!(outcome.status, FrameStatus::Success) => {
                if let Err(commit) = host.commit_frame() {
                    return match host.revert_frame() {
                        Ok(()) => Err(DtvmError::Checkpoint(commit)),
                        Err(rollback) => Err(DtvmError::CommitRollback { commit, rollback }),
                    };
                }
                Ok(outcome)
            }
            Ok(outcome) => {
                host.revert_frame().map_err(DtvmError::Checkpoint)?;
                Ok(outcome)
            }
            Err(error) => {
                host.revert_frame().map_err(DtvmError::Checkpoint)?;
                Err(error)
            }
        }
    }
}

impl Drop for Dtvm {
    fn drop(&mut self) {
        // SAFETY: the VM is live exactly until this Drop; the Library field is
        // still loaded while metrics snapshot and destroy execute.
        unsafe {
            if let Some(phase_metrics) = &self.phase_metrics {
                let mut metrics = DtvmEvmcPhaseMetrics::request();
                let status = DtvmPhaseMetricsStatus::from_code((phase_metrics.get)(
                    self.vm.as_ptr(),
                    &mut metrics,
                ));
                phase_metrics.collector.record(
                    status,
                    (status == DtvmPhaseMetricsStatus::Success).then_some(metrics),
                );
            }
            if let Some(destroy) = self.vm.as_ref().destroy {
                destroy(self.vm.as_ptr());
            }
        }
    }
}

struct DtvmPhaseMetricsInstance {
    get: GetDtvmEvmcPhaseMetricsFn,
    collector: DtvmPhaseMetricsCollector,
}

struct DtvmHotMetricsInstance {
    get: GetDtvmEvmcHotMetricsFn,
}

struct EvmoneAdvancedDiagnosticMetricsInstance {
    get: GetEvmoneAdvancedDiagnosticMetricsFn,
}

fn initialize_phase_metrics(
    library: &Library,
    vm: NonNull<EvmcVm>,
    collector: DtvmPhaseMetricsCollector,
) -> Option<DtvmPhaseMetricsInstance> {
    // SAFETY: the exact C signatures and POD layout are pinned by the DTVM
    // diagnostic ABI. Copied function pointers remain valid while `library`
    // is held by `Dtvm`.
    let reset = unsafe {
        library
            .get::<ResetDtvmEvmcPhaseMetricsFn>(DTVM_RESET_EVMC_PHASE_METRICS_SYMBOL)
            .ok()
            .map(|symbol| *symbol)
    };
    // SAFETY: same diagnostic ABI contract as above.
    let get = unsafe {
        library
            .get::<GetDtvmEvmcPhaseMetricsFn>(DTVM_GET_EVMC_PHASE_METRICS_SYMBOL)
            .ok()
            .map(|symbol| *symbol)
    };
    let (reset, get) = match (reset, get) {
        (Some(reset), Some(get)) => (reset, get),
        (None, None) => {
            collector.record(DtvmPhaseMetricsStatus::Disabled, None);
            return None;
        }
        _ => {
            collector.record(DtvmPhaseMetricsStatus::Incompatible, None);
            return None;
        }
    };
    // SAFETY: the VM is quiescent immediately after creation and option setup.
    let status = DtvmPhaseMetricsStatus::from_code(unsafe {
        reset(
            vm.as_ptr(),
            DTVM_EVMC_PHASE_METRICS_VERSION,
            DTVM_EVMC_PHASE_METRICS_STRUCT_SIZE,
        )
    });
    if status != DtvmPhaseMetricsStatus::Success {
        collector.record(status, None);
        return None;
    }
    Some(DtvmPhaseMetricsInstance { get, collector })
}

fn validate_request(revision: i32, message: &Message) -> Result<(), DtvmError> {
    if revision != EVMC_OSAKA {
        return Err(DtvmError::UnsupportedRevision(revision));
    }
    match message.kind {
        EVMC_CALL => {
            if message.flags & !EVMC_DELEGATED != 0 {
                return Err(DtvmError::UnsupportedMessage(
                    "top-level static or unknown CALL flags are not enabled",
                ));
            }
            if message.flags == 0 && message.code_address != message.recipient {
                return Err(DtvmError::UnsupportedMessage(
                    "ordinary CALL code address differs from recipient",
                ));
            }
        }
        EVMC_CREATE => {
            if message.flags != 0 {
                return Err(DtvmError::UnsupportedMessage(
                    "top-level CREATE flags must be zero",
                ));
            }
            if !message.input.is_empty() {
                return Err(DtvmError::UnsupportedMessage(
                    "top-level CREATE calldata must be empty",
                ));
            }
            if message.code_address != Address::default() {
                return Err(DtvmError::UnsupportedMessage(
                    "top-level CREATE code address must be zero",
                ));
            }
        }
        EVMC_CREATE2 => {
            return Err(DtvmError::UnsupportedMessage(
                "top-level CREATE2 is not enabled",
            ));
        }
        EVMC_EOFCREATE => {
            return Err(DtvmError::UnsupportedMessage(
                "top-level EOFCREATE is not enabled",
            ));
        }
        _ => {
            return Err(DtvmError::UnsupportedMessage(
                "unknown top-level message kind",
            ));
        }
    }
    if message.input.len() > isize::MAX as usize {
        return Err(DtvmError::UnsupportedMessage("input exceeds address space"));
    }
    Ok(())
}

fn validate_result(result: &EvmcResult, initial_gas: i64) -> Result<(), DtvmError> {
    if result.output_size != 0 && result.output_data.is_null() {
        return Err(DtvmError::ResultInvariant(
            "non-empty output has a null pointer",
        ));
    }
    if result.gas_left < 0 || result.gas_left > initial_gas {
        return Err(DtvmError::ResultInvariant("gas_left is out of range"));
    }
    match result.status_code {
        // Frame-local refunds are signed in REVM. A successful EVMC frame may
        // therefore carry a negative refund; transaction-level accounting is
        // responsible for applying the final cap.
        EVMC_SUCCESS => {}
        EVMC_REVERT => {
            if result.gas_refund != 0 {
                return Err(DtvmError::ResultInvariant("REVERT has a refund"));
            }
        }
        _ => {
            if result.gas_left != 0 || result.gas_refund != 0 {
                return Err(DtvmError::ResultInvariant(
                    "non-success/revert status retained gas or refund",
                ));
            }
        }
    }
    Ok(())
}

fn copy_output(result: &EvmcResult) -> Result<Vec<u8>, DtvmError> {
    if result.output_size == 0 {
        return Ok(Vec::new());
    }
    if result.output_data.is_null() {
        return Err(DtvmError::ResultInvariant(
            "non-empty output has a null pointer",
        ));
    }
    if result.output_size > isize::MAX as usize {
        return Err(DtvmError::ResultInvariant(
            "output length exceeds Rust slice bounds",
        ));
    }
    // SAFETY: EVMC result owns a readable buffer until release.
    Ok(unsafe { slice::from_raw_parts(result.output_data, result.output_size) }.to_vec())
}

fn is_consensus_halt(status: i32) -> bool {
    matches!(
        status,
        EVMC_OUT_OF_GAS
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

struct ResultGuard(EvmcResult);

impl Drop for ResultGuard {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            // SAFETY: release belongs to this exact result and is invoked once.
            unsafe { release(&self.0) };
        }
    }
}

fn null_if_empty(bytes: &[u8]) -> *const u8 {
    if bytes.is_empty() {
        ptr::null()
    } else {
        bytes.as_ptr()
    }
}

fn c_metadata(value: *const core::ffi::c_char, field: &'static str) -> Result<String, DtvmError> {
    if value.is_null() {
        return Err(DtvmError::InvalidMetadata(field));
    }
    // SAFETY: EVMC requires null-terminated UTF-8 metadata.
    let value = unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| DtvmError::InvalidMetadata(field))?;
    if value.is_empty() {
        return Err(DtvmError::InvalidMetadata(field));
    }
    Ok(value.to_owned())
}

fn set_mandatory_option(
    vm: NonNull<EvmcVm>,
    set_option: crate::ffi::SetOptionFn,
    name: &'static str,
    value: &'static str,
) -> Result<(), DtvmError> {
    let c_name = CString::new(name).expect("static option name contains no NUL");
    let c_value = CString::new(value).expect("static option value contains no NUL");
    // SAFETY: mandatory vtable function and live VM; C strings are terminated.
    let result = unsafe { set_option(vm.as_ptr(), c_name.as_ptr(), c_value.as_ptr()) };
    if result == EVMC_SET_OPTION_SUCCESS {
        Ok(())
    } else {
        Err(destroy_with_error(
            vm,
            DtvmError::OptionRejected {
                name,
                value,
                result,
            },
        ))
    }
}

fn set_mandatory_options(
    vm: NonNull<EvmcVm>,
    set_option: crate::ffi::SetOptionFn,
    backend: SubjectBackend,
) -> Result<(), DtvmError> {
    for &(name, value) in backend.mandatory_options() {
        set_mandatory_option(vm, set_option, name, value)?;
    }
    Ok(())
}

/// Applies `name=value` pairs from a comma-separated spec through the same
/// checked EVMC `set_option` path as the mandatory options. Empty entries are
/// ignored; anything else must parse and be accepted or startup fails.
fn apply_extra_options(
    vm: NonNull<EvmcVm>,
    set_option: crate::ffi::SetOptionFn,
    spec: &str,
) -> Result<(), DtvmError> {
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((name, value)) = entry.split_once('=') else {
            return Err(destroy_with_error(
                vm,
                DtvmError::ExtraOptionMalformed {
                    entry: entry.to_owned(),
                },
            ));
        };
        let c_name =
            CString::new(name).expect("extra option name contains no NUL");
        let c_value =
            CString::new(value).expect("extra option value contains no NUL");
        // SAFETY: mandatory vtable function and live VM; C strings terminated.
        let result =
            unsafe { set_option(vm.as_ptr(), c_name.as_ptr(), c_value.as_ptr()) };
        if result != EVMC_SET_OPTION_SUCCESS {
            return Err(destroy_with_error(
                vm,
                DtvmError::ExtraOptionRejected {
                    name: name.to_owned(),
                    value: value.to_owned(),
                    result,
                },
            ));
        }
    }
    Ok(())
}

fn set_extra_options_from_env(
    vm: NonNull<EvmcVm>,
    set_option: crate::ffi::SetOptionFn,
) -> Result<(), DtvmError> {
    match std::env::var(SUBJECT_EXTRA_OPTIONS_ENV) {
        Ok(spec) => apply_extra_options(vm, set_option, &spec),
        Err(_) => Ok(()),
    }
}

fn destroy_with_error(vm: NonNull<EvmcVm>, error: DtvmError) -> DtvmError {
    destroy_if_present(vm);
    error
}

fn destroy_if_present(vm: NonNull<EvmcVm>) {
    // SAFETY: used only on factory-created instances not yet wrapped by Dtvm.
    unsafe {
        if let Some(destroy) = vm.as_ref().destroy {
            destroy(vm.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::{EVMC_FAILURE, EvmcAddress};
    use crate::host::{Account, TxContextOwned, WitnessBackend};
    use std::sync::Mutex;

    static CAPTURED_OPTIONS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn capture_option(
        _vm: *mut EvmcVm,
        name: *const core::ffi::c_char,
        value: *const core::ffi::c_char,
    ) -> i32 {
        // SAFETY: `set_mandatory_option` passes live NUL-terminated C strings.
        let name = unsafe { CStr::from_ptr(name) }
            .to_str()
            .expect("UTF-8 option name");
        // SAFETY: `set_mandatory_option` passes live NUL-terminated C strings.
        let value = unsafe { CStr::from_ptr(value) }
            .to_str()
            .expect("UTF-8 option value");
        CAPTURED_OPTIONS
            .lock()
            .expect("capture lock")
            .push((name.to_owned(), value.to_owned()));
        EVMC_SET_OPTION_SUCCESS
    }

    #[test]
    fn mandatory_options_are_fixed_by_subject_backend() {
        for (backend, expected_options) in [
            (
                SubjectBackend::DtvmEager,
                vec![
                    ("mode".to_string(), "multipass".to_string()),
                    ("enable_gas_metering".to_string(), "true".to_string()),
                    ("profile_guided_jit".to_string(), "false".to_string()),
                ],
            ),
            (
                SubjectBackend::DtvmProfileGuided,
                vec![
                    ("mode".to_string(), "multipass".to_string()),
                    ("enable_gas_metering".to_string(), "true".to_string()),
                    ("profile_guided_jit".to_string(), "true".to_string()),
                ],
            ),
            (
                SubjectBackend::EvmoneAdvanced,
                vec![("advanced".to_string(), String::new())],
            ),
        ] {
            CAPTURED_OPTIONS.lock().expect("capture lock").clear();
            let mut raw_vm = EvmcVm {
                abi_version: EVMC_ABI_VERSION,
                name: ptr::null(),
                version: ptr::null(),
                destroy: None,
                execute: None,
                get_capabilities: None,
                set_option: Some(capture_option),
            };

            set_mandatory_options(NonNull::from(&mut raw_vm), capture_option, backend)
                .expect("mandatory options");

            assert_eq!(
                *CAPTURED_OPTIONS.lock().expect("capture lock"),
                expected_options
            );
        }
    }

    #[test]
    fn extra_options_parse_apply_and_fail_loudly() {
        let make_vm = || EvmcVm {
            abi_version: EVMC_ABI_VERSION,
            name: ptr::null(),
            version: ptr::null(),
            destroy: None,
            execute: None,
            get_capabilities: None,
            set_option: Some(capture_option),
        };

        // Well-formed spec applies every pair in order via set_option.
        CAPTURED_OPTIONS.lock().expect("capture lock").clear();
        let mut raw_vm = make_vm();
        apply_extra_options(
            NonNull::from(&mut raw_vm),
            capture_option,
            " code_cache_dir=/tmp/cc , code_cache_mode=rw ,",
        )
        .expect("extra options apply");
        assert_eq!(
            *CAPTURED_OPTIONS.lock().expect("capture lock"),
            vec![
                ("code_cache_dir".to_string(), "/tmp/cc".to_string()),
                ("code_cache_mode".to_string(), "rw".to_string()),
            ]
        );

        // Malformed entry fails loudly rather than being skipped.
        let mut raw_vm = make_vm();
        let error = apply_extra_options(
            NonNull::from(&mut raw_vm),
            capture_option,
            "code_cache_mode",
        )
        .expect_err("malformed entry must be rejected");
        assert!(matches!(error, DtvmError::ExtraOptionMalformed { .. }));

        // Empty spec is a no-op.
        CAPTURED_OPTIONS.lock().expect("capture lock").clear();
        let mut raw_vm = make_vm();
        apply_extra_options(NonNull::from(&mut raw_vm), capture_option, "  ")
            .expect("empty spec");
        assert!(CAPTURED_OPTIONS.lock().expect("capture lock").is_empty());
    }

    #[test]
    fn phase_metrics_collector_sums_complete_successful_snapshots() {
        let collector = DtvmPhaseMetricsCollector::default();
        let first = DtvmEvmcPhaseMetrics {
            top_level_execute_count: 2,
            top_level_execute_wall_ns: 100,
            synchronous_jit_compile_attempt_count: 1,
            synchronous_jit_compile_success_count: 1,
            synchronous_jit_compile_wall_ns: 60,
            non_compile_residual_ns: 40,
            profile_guided_jit_trigger_count: 0,
            module_cache_hit_count: 4,
            ..DtvmEvmcPhaseMetrics::request()
        };
        let second = DtvmEvmcPhaseMetrics {
            top_level_execute_count: 3,
            top_level_execute_wall_ns: 200,
            synchronous_jit_compile_attempt_count: 2,
            synchronous_jit_compile_success_count: 2,
            synchronous_jit_compile_wall_ns: 120,
            non_compile_residual_ns: 80,
            module_cache_hit_count: 5,
            ..first
        };

        collector.record(DtvmPhaseMetricsStatus::Success, Some(first));
        collector.record(DtvmPhaseMetricsStatus::Success, Some(second));

        assert_eq!(
            collector.report(),
            DtvmPhaseMetricsReport {
                status: DtvmPhaseMetricsStatus::Success,
                metrics: Some(DtvmEvmcPhaseMetrics {
                    top_level_execute_count: 5,
                    top_level_execute_wall_ns: 300,
                    synchronous_jit_compile_attempt_count: 3,
                    synchronous_jit_compile_success_count: 3,
                    synchronous_jit_compile_wall_ns: 180,
                    non_compile_residual_ns: 120,
                    profile_guided_jit_trigger_count: 0,
                    module_cache_hit_count: 9,
                    ..DtvmEvmcPhaseMetrics::request()
                }),
            }
        );
    }

    #[test]
    fn phase_metrics_request_is_the_v2_192_byte_layout() {
        let request = DtvmEvmcPhaseMetrics::request();
        assert_eq!(request.version, 2);
        assert_eq!(request.struct_size, 192);
        assert_eq!(core::mem::size_of::<DtvmEvmcPhaseMetrics>(), 192);
    }

    #[test]
    fn phase_metrics_collector_reports_disabled_and_rejects_mixed_statuses() {
        let collector = DtvmPhaseMetricsCollector::default();
        assert_eq!(
            collector.report(),
            DtvmPhaseMetricsReport {
                status: DtvmPhaseMetricsStatus::Disabled,
                metrics: None,
            }
        );

        collector.record(DtvmPhaseMetricsStatus::Disabled, None);
        assert_eq!(collector.report().status, DtvmPhaseMetricsStatus::Disabled);
        collector.record(
            DtvmPhaseMetricsStatus::Success,
            Some(DtvmEvmcPhaseMetrics::request()),
        );
        assert_eq!(
            collector.report(),
            DtvmPhaseMetricsReport {
                status: DtvmPhaseMetricsStatus::Inconsistent,
                metrics: None,
            }
        );
    }

    #[test]
    fn subject_backend_allowlist_is_exact_and_fail_closed() {
        assert_eq!(
            SubjectBackend::parse("dtvm-eager").unwrap(),
            SubjectBackend::DtvmEager
        );
        assert_eq!(
            SubjectBackend::parse("dtvm-profile-guided").unwrap(),
            SubjectBackend::DtvmProfileGuided
        );
        assert_eq!(
            SubjectBackend::parse("evmone-advanced").unwrap(),
            SubjectBackend::EvmoneAdvanced
        );
        assert_eq!(
            SubjectBackend::parse("evmone-baseline").unwrap(),
            SubjectBackend::EvmoneBaseline
        );
        assert!(matches!(
            SubjectBackend::parse(""),
            Err(DtvmError::InvalidSubjectBackend(value)) if value.is_empty()
        ));
        assert!(matches!(
            SubjectBackend::parse("evmone"),
            Err(DtvmError::InvalidSubjectBackend(value)) if value == "evmone"
        ));
        assert_eq!(
            SubjectBackend::DtvmEager.factory_symbol(),
            b"evmc_create_dtvmapi\0"
        );
        assert_eq!(
            SubjectBackend::DtvmProfileGuided.factory_symbol(),
            b"evmc_create_dtvmapi\0"
        );
        assert_eq!(
            SubjectBackend::EvmoneAdvanced.factory_symbol(),
            b"evmc_create_evmone\0"
        );
        assert_eq!(
            SubjectBackend::EvmoneBaseline.factory_symbol(),
            b"evmc_create_evmone\0"
        );
        assert_eq!(SubjectBackend::DtvmEager.expected_vm_name(), "dtvm");
        assert_eq!(SubjectBackend::DtvmProfileGuided.expected_vm_name(), "dtvm");
        assert_eq!(SubjectBackend::EvmoneAdvanced.expected_vm_name(), "evmone");
        assert_eq!(SubjectBackend::EvmoneBaseline.expected_vm_name(), "evmone");
        assert!(SubjectBackend::DtvmEager.requires_strict_address_cache());
        assert!(SubjectBackend::DtvmProfileGuided.requires_strict_address_cache());
        assert!(!SubjectBackend::EvmoneAdvanced.requires_strict_address_cache());
        assert!(!SubjectBackend::EvmoneBaseline.requires_strict_address_cache());
        // The two evmone legs differ only in whether `advanced` is opted into.
        assert_eq!(
            SubjectBackend::EvmoneAdvanced.mandatory_options(),
            &[("advanced", "")]
        );
        assert!(SubjectBackend::EvmoneBaseline.mandatory_options().is_empty());
    }

    #[test]
    fn rejects_non_osaka_without_touching_host() {
        let mut backend = WitnessBackend::default();
        let host = HostContext::new(&mut backend, TxContextOwned::default());
        let message = Message::ordinary(
            Address([1; 20]),
            Address([2; 20]),
            Vec::new(),
            Word::ZERO,
            100,
        );
        assert!(matches!(
            validate_request(13, &message),
            Err(DtvmError::UnsupportedRevision(13))
        ));
        assert!(host.audit().is_empty());
        assert!(host.fault().is_none());
    }

    #[test]
    fn result_invariants_reject_backend_failure_with_gas() {
        let result = EvmcResult {
            status_code: EVMC_FAILURE,
            gas_left: 1,
            gas_refund: 0,
            output_data: ptr::null(),
            output_size: 0,
            release: None,
            create_address: EvmcAddress::default(),
            padding: [0; 4],
        };
        assert!(matches!(
            validate_result(&result, 100),
            Err(DtvmError::ResultInvariant(_))
        ));
    }

    #[test]
    fn result_invariants_accept_negative_success_refund() {
        let result = EvmcResult {
            status_code: EVMC_SUCCESS,
            gas_left: 1,
            gas_refund: -1,
            output_data: ptr::null(),
            output_size: 0,
            release: None,
            create_address: EvmcAddress::default(),
            padding: [0; 4],
        };
        assert!(validate_result(&result, 100).is_ok());
    }

    #[test]
    fn witness_account_helper_hashes_code() {
        let account = Account::new(Word::ZERO, vec![0x00]);
        assert_ne!(account.code_hash(), Word::ZERO);
    }

    #[test]
    fn eip7702_delegated_code_is_accepted_but_static_and_plain_mismatch_fail_closed() {
        let recipient = Address([0x11; 20]);
        let sender = Address([0x22; 20]);
        let mut delegated = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 100_000);
        delegated.flags = EVMC_DELEGATED;
        delegated.code_address = Address([0x33; 20]);
        assert!(validate_request(EVMC_OSAKA, &delegated).is_ok());

        delegated.flags = crate::ffi::EVMC_STATIC;
        assert!(matches!(
            validate_request(EVMC_OSAKA, &delegated),
            Err(DtvmError::UnsupportedMessage(
                "top-level static or unknown CALL flags are not enabled"
            ))
        ));

        delegated.flags = 0;
        assert!(matches!(
            validate_request(EVMC_OSAKA, &delegated),
            Err(DtvmError::UnsupportedMessage(
                "ordinary CALL code address differs from recipient"
            ))
        ));
    }

    #[test]
    fn top_level_create_accepts_only_consistent_create_fields() {
        let recipient = Address([0x11; 20]);
        let sender = Address([0x22; 20]);
        let create = Message::create(recipient, sender, Word::from_u64(7), 100_000);
        assert!(validate_request(EVMC_OSAKA, &create).is_ok());

        for kind in [EVMC_CREATE2, EVMC_EOFCREATE, 99] {
            let mut invalid = create.clone();
            invalid.kind = kind;
            assert!(matches!(
                validate_request(EVMC_OSAKA, &invalid),
                Err(DtvmError::UnsupportedMessage(_))
            ));
        }

        let mut invalid = create.clone();
        invalid.flags = EVMC_DELEGATED;
        assert!(matches!(
            validate_request(EVMC_OSAKA, &invalid),
            Err(DtvmError::UnsupportedMessage(
                "top-level CREATE flags must be zero"
            ))
        ));

        let mut invalid = create.clone();
        invalid.input.push(0x01);
        assert!(matches!(
            validate_request(EVMC_OSAKA, &invalid),
            Err(DtvmError::UnsupportedMessage(
                "top-level CREATE calldata must be empty"
            ))
        ));

        let mut invalid = create;
        invalid.code_address = recipient;
        assert!(matches!(
            validate_request(EVMC_OSAKA, &invalid),
            Err(DtvmError::UnsupportedMessage(
                "top-level CREATE code address must be zero"
            ))
        ));
    }
}
