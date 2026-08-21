//! Fail-closed EVMC ABI 12 adapter core for DTVM.
//!
//! The core executes Osaka top-level CALL and CREATE-initcode frames, including
//! delegated-code CALLs. Its client backend owns state and child-frame
//! lifecycle semantics; the Reth backend implements CALL, STATICCALL,
//! DELEGATECALL, CALLCODE, CREATE, CREATE2, and SELFDESTRUCT. EOFCREATE,
//! unknown revisions or flags, and capabilities omitted by another backend
//! remain explicitly fail-closed.

pub mod ffi;
pub mod host;
pub mod vm;

pub use host::{
    AccessEvent, Account, Address, HostBackend, HostContext, HostFault, LogEntry,
    NestedCallPreparation, NestedCallRequest, NestedCallResult, TxContextOwned, WitnessBackend,
    Word,
};
pub use vm::{
    Dtvm, DtvmError, DtvmEvmcHotMetrics, DtvmEvmcPhaseMetrics, DtvmPhaseMetricsCollector,
    DtvmPhaseMetricsReport, DtvmPhaseMetricsStatus, EvmoneAdvancedDiagnosticMetrics, FrameOutcome,
    FrameStatus, Message, SubjectBackend, SUBJECT_BACKEND_ENV,
};
