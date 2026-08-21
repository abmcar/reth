//! Strict transaction-level Reth/DTVM adapter experiment.

pub mod journal_host;
pub mod reth_evm;
pub mod strict_db;

pub use journal_host::JournalHost;
pub use reth_dtvm_adapter::{
    DtvmEvmcHotMetrics, DtvmEvmcPhaseMetrics, DtvmPhaseMetricsReport, DtvmPhaseMetricsStatus,
    EvmoneAdvancedDiagnosticMetrics, SubjectBackend,
};
pub use reth_evm::{
    DtvmEvm, DtvmEvmFactory, SubjectEvmFactory, STORAGE_LOG_RETURN, SUPPORTED_TX_GAS_LIMIT,
};
pub use strict_db::{DbAccess, StrictDb, StrictDbError};
