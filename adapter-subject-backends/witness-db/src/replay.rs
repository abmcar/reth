use crate::{WitnessBundle, WitnessDb, WitnessImportError};
use alloy_evm::eth::EthEvmFactory;
use alloy_primitives::{Address, B256};
use reth_chainspec::MAINNET;
use reth_consensus::{Consensus, HeaderValidator};
use reth_dtvm_transaction_adapter::{
    DbAccess, DtvmEvmcHotMetrics as AdapterDtvmEvmcHotMetrics,
    DtvmEvmcPhaseMetrics as AdapterDtvmEvmcPhaseMetrics,
    DtvmPhaseMetricsReport as AdapterDtvmPhaseMetricsReport,
    EvmoneAdvancedDiagnosticMetrics as AdapterEvmoneAdvancedDiagnosticMetrics, SubjectBackend,
    SubjectEvmFactory,
};
use reth_ethereum_consensus::{validate_block_post_execution, EthBeaconConsensus};
use reth_ethereum_primitives::Block;
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_evm_ethereum::{revm_spec, EthEvmConfig};
use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};
use revm::{
    database::BundleState,
    handler::execution_metrics::{self, RunExecLoopMetrics as AdapterRunExecLoopMetrics},
    primitives::hardfork::SpecId,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    error::Error,
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    time::Instant,
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostExecutionCommitments {
    pub gas_used: bool,
    pub receipts_root: bool,
    pub logs_bloom: bool,
    pub requests_hash: bool,
    pub blob_gas_used: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayPhaseWallTimeNs {
    pub prepare_import_preflight: u64,
    pub reth_revm_execute: Option<u64>,
    pub reth_subject_execute: Option<u64>,
    pub validation_roots: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunExecLoopMetrics {
    pub call_count: u64,
    pub wall_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayMode {
    Differential,
    ReferenceOnly,
    SubjectOnly,
}

impl ReplayMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "differential" => Some(Self::Differential),
            "reference-only" => Some(Self::ReferenceOnly),
            "subject-only" => Some(Self::SubjectOnly),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Differential => "differential",
            Self::ReferenceOnly => "reference-only",
            Self::SubjectOnly => "subject-only",
        }
    }

    const fn runs_subject(self) -> bool {
        matches!(self, Self::Differential | Self::SubjectOnly)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayReport {
    pub replay_mode: ReplayMode,
    pub subject_backend: Option<String>,
    pub subject_vm_create_count: Option<u64>,
    pub dtvm_phase_metrics: Option<DtvmPhaseMetricsReport>,
    pub dtvm_hot_metrics: Option<DtvmHotMetricsObservation>,
    pub evmone_advanced_metrics: Option<EvmoneAdvancedMetricsObservation>,
    pub reth_revm_run_exec_loop: Option<RunExecLoopMetrics>,
    pub reth_subject_run_exec_loop: Option<RunExecLoopMetrics>,
    pub differential_match: Option<bool>,
    pub raw_bound: bool,
    pub pre_execution_commitments: bool,
    pub post_execution_commitments: PostExecutionCommitments,
    pub pre_state_root: B256,
    pub pre_state_root_verified: bool,
    pub post_state_root: B256,
    pub post_state_root_verified: bool,
    pub block_number: u64,
    pub block_hash: B256,
    pub raw_block_bytes: usize,
    pub transaction_count: usize,
    pub receipt_count: usize,
    pub gas_used: u64,
    pub blob_gas_used: u64,
    /// Sequential, non-overlapping secondary timings. External process elapsed
    /// remains the formal end-to-end measurement.
    pub phase_wall_time_ns: ReplayPhaseWallTimeNs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DtvmHotMetricsObservation {
    pub before: DtvmHotMetricsSnapshot,
    pub after: DtvmHotMetricsSnapshot,
    pub delta: DtvmHotMetricsDelta,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DtvmHotMetricsSnapshot {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DtvmHotMetricsDelta {
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
    pub module_cache_entry_count_before: u64,
    pub module_cache_entry_count_after: u64,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmoneAdvancedMetricsObservation {
    pub before: EvmoneAdvancedMetricsSnapshot,
    pub after: EvmoneAdvancedMetricsSnapshot,
    pub delta: EvmoneAdvancedMetricsDelta,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmoneAdvancedMetricsSnapshot {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmoneAdvancedMetricsDelta {
    pub top_level_execute_count: u64,
    pub top_level_execute_wall_ns: u64,
    pub advanced_analysis_count: u64,
    pub advanced_analysis_wall_ns: u64,
    pub advanced_state_setup_count: u64,
    pub advanced_state_setup_wall_ns: u64,
    pub advanced_core_execute_count: u64,
    pub advanced_core_execute_wall_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DtvmPhaseMetricsReport {
    pub status: String,
    pub status_code: i32,
    pub metrics: Option<DtvmEvmcPhaseMetrics>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DtvmEvmcPhaseMetrics {
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

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("invalid witness bundle JSON: {0}")]
    InvalidBundleJson(String),
    #[error("reference witness import failed: {0}")]
    ReferenceWitnessImport(#[source] WitnessImportError),
    #[error("subject witness import failed: {0}")]
    SubjectWitnessImport(#[source] WitnessImportError),
    #[error("strict replay requires a verified targetBlock")]
    MissingTargetBlock,
    #[error("raw target block decode failed: {0}")]
    RawBlockDecode(String),
    #[error("raw target block has trailing bytes")]
    RawBlockTrailingBytes,
    #[error("sender recovery failed for at least one transaction")]
    SenderRecovery,
    #[error("adapterUnsupported at transaction {transaction_index}: {reason}")]
    AdapterUnsupported {
        transaction_index: usize,
        reason: &'static str,
    },
    #[error("strict replay requires mainnet Osaka, got {actual:?}")]
    UnsupportedSpec { actual: SpecId },
    #[error("strict replay rejects block access list headers")]
    BlockAccessListUnsupported,
    #[error("strict replay rejects slot-number headers")]
    SlotNumberUnsupported,
    #[error("header consensus preflight failed: {0}")]
    HeaderPreflight(String),
    #[error("header-against-parent consensus preflight failed: {0}")]
    HeaderAgainstParent(String),
    #[error("block body consensus preflight failed: {0}")]
    BlockPreflight(String),
    #[error("subject provenance validation failed: {0}")]
    SubjectProvenance(String),
    #[error("hot-cache batch setup failed: {0}")]
    HotBatchSetup(String),
    #[error("hot-cache metrics are not monotonic: {0}")]
    HotMetricsNonMonotonic(&'static str),
    #[error("run_exec_loop metrics are not monotonic")]
    RunExecLoopMetricsNonMonotonic,
    #[error("evmone advanced metrics are not monotonic: {0}")]
    EvmoneMetricsNonMonotonic(&'static str),
    #[error("referenceWitnessIncomplete after {access_count} database accesses: {message}")]
    ReferenceWitnessIncomplete {
        message: String,
        access_count: usize,
    },
    #[error("reference execution failed after {access_count} database accesses: {message}")]
    ReferenceExecution {
        message: String,
        access_count: usize,
    },
    #[error("subjectUnprovenExtraAccess after {access_count} database accesses: {message}")]
    SubjectUnprovenExtraAccess {
        message: String,
        access_count: usize,
    },
    #[error("subject execution failed after {access_count} database accesses: {message}")]
    SubjectExecution {
        message: String,
        access_count: usize,
    },
    #[error("reference post-execution validation failed: {0}")]
    ReferencePostValidation(String),
    #[error("subject post-execution validation failed: {0}")]
    SubjectPostValidation(String),
    #[error("reference post-state root verification failed: {0}")]
    ReferencePostStateRoot(#[source] WitnessImportError),
    #[error("subject post-state root verification failed: {0}")]
    SubjectPostStateRoot(#[source] WitnessImportError),
    #[error("reference blob gas mismatch: expected {expected}, got {actual}")]
    ReferenceBlobGasMismatch { expected: u64, actual: u64 },
    #[error("subject blob gas mismatch: expected {expected}, got {actual}")]
    SubjectBlobGasMismatch { expected: u64, actual: u64 },
    #[error("reference and subject BlockExecutionResult differ")]
    ExecutionResultMismatch,
    #[error(
        "reference and subject strict database access sequences differ at index {index}: \
         reference={reference}, subject={subject}; reference length={reference_len}, \
         subject length={subject_len}"
    )]
    AccessSequenceMismatch {
        index: usize,
        reference: String,
        subject: String,
        reference_len: usize,
        subject_len: usize,
    },
    #[error("reference and subject BundleState semantics differ")]
    BundleStateMismatch,
}

#[derive(Clone, Copy)]
enum BatchMetricsKind {
    DtvmHot,
    EvmoneAdvanced,
}

/// Single-threaded batch session retaining one eager DTVM instance and its code cache.
///
/// Every [`replay_json`](Self::replay_json) call still constructs fresh witness databases,
/// executors, journals, and hosts. Only the EVMC VM behind the subject factory survives.
pub struct ReplayBatchSession {
    subject: ReplaySubject,
    batch_metrics: Option<BatchMetricsKind>,
}

impl ReplayBatchSession {
    /// Loads and verifies the configured subject, requires metrics ABI v2, and resets its
    /// monotonic counters before the first cold-population pass.
    pub fn from_env() -> Result<Self, ReplayError> {
        Self::from_env_with_diagnostic_metrics(true)
    }

    /// Loads one production DTVM whose diagnostic ABI is absent. The EVMC VM
    /// and code cache remain alive for the complete fixed batch.
    pub fn from_env_production() -> Result<Self, ReplayError> {
        Self::from_env_with_diagnostic_metrics(false)
    }

    fn from_env_with_diagnostic_metrics(
        require_diagnostic_metrics: bool,
    ) -> Result<Self, ReplayError> {
        if execution_metrics::is_enabled() {
            return Err(ReplayError::HotBatchSetup(
                "run_exec_loop collection is already enabled on this thread".to_string(),
            ));
        }
        let subject = ReplaySubject::from_env()?;
        if subject.backend != SubjectBackend::DtvmEager {
            return Err(ReplayError::HotBatchSetup(format!(
                "RETH_SUBJECT_BACKEND must be dtvm-eager, got {}",
                subject.backend.as_str()
            )));
        }
        let batch_metrics = if require_diagnostic_metrics {
            subject
                .factory
                .require_hot_metrics_v2()
                .map_err(ReplayError::HotBatchSetup)?;
            Some(BatchMetricsKind::DtvmHot)
        } else {
            let report = subject.factory.dtvm_phase_metrics().ok_or_else(|| {
                ReplayError::HotBatchSetup(
                    "production DTVM phase-metrics state is unavailable".to_string(),
                )
            })?;
            if report.status.as_str() != "disabled" || report.metrics.is_some() {
                return Err(ReplayError::HotBatchSetup(format!(
                    "production DTVM must have phase metrics disabled, got {}",
                    report.status.as_str()
                )));
            }
            None
        };
        if subject.factory.vm_create_count() != 1 {
            return Err(ReplayError::HotBatchSetup(format!(
                "expected exactly one EVMC VM after setup, got {}",
                subject.factory.vm_create_count()
            )));
        }
        execution_metrics::enable();
        Ok(Self {
            subject,
            batch_metrics,
        })
    }

    pub fn replay_json(&self, json: &[u8]) -> Result<ReplayReport, ReplayError> {
        self.replay_json_with_mode(json, ReplayMode::Differential)
    }

    pub fn replay_json_with_mode(
        &self,
        json: &[u8],
        mode: ReplayMode,
    ) -> Result<ReplayReport, ReplayError> {
        let prepare_started = Instant::now();
        let bundle = serde_json::from_slice(json)
            .map_err(|error| ReplayError::InvalidBundleJson(error.to_string()))?;
        replay_bundle_started(
            bundle,
            mode,
            prepare_started,
            Some(&self.subject),
            self.batch_metrics,
        )
    }

    pub fn subject_vm_create_count(&self) -> u64 {
        self.subject.factory.vm_create_count()
    }

    pub fn phase_metrics_disabled(&self) -> bool {
        self.subject
            .factory
            .dtvm_phase_metrics()
            .is_some_and(|report| report.status.as_str() == "disabled" && report.metrics.is_none())
    }
}

/// Single-threaded batch session retaining one instrumented evmone advanced VM.
pub struct ReplayEvmoneBatchSession {
    subject: ReplaySubject,
}

impl ReplayEvmoneBatchSession {
    pub fn from_env() -> Result<Self, ReplayError> {
        if execution_metrics::is_enabled() {
            return Err(ReplayError::HotBatchSetup(
                "run_exec_loop collection is already enabled on this thread".to_string(),
            ));
        }
        let subject = ReplaySubject::from_env()?;
        if subject.backend != SubjectBackend::EvmoneAdvanced {
            return Err(ReplayError::HotBatchSetup(format!(
                "RETH_SUBJECT_BACKEND must be evmone-advanced, got {}",
                subject.backend.as_str()
            )));
        }
        subject
            .factory
            .require_evmone_diagnostic_metrics_v1()
            .map_err(ReplayError::HotBatchSetup)?;
        if subject.factory.vm_create_count() != 1 {
            return Err(ReplayError::HotBatchSetup(format!(
                "expected exactly one EVMC VM after setup, got {}",
                subject.factory.vm_create_count()
            )));
        }
        execution_metrics::enable();
        Ok(Self { subject })
    }

    pub fn replay_json(&self, json: &[u8]) -> Result<ReplayReport, ReplayError> {
        self.replay_json_with_mode(json, ReplayMode::Differential)
    }

    pub fn replay_json_with_mode(
        &self,
        json: &[u8],
        mode: ReplayMode,
    ) -> Result<ReplayReport, ReplayError> {
        let prepare_started = Instant::now();
        let bundle = serde_json::from_slice(json)
            .map_err(|error| ReplayError::InvalidBundleJson(error.to_string()))?;
        replay_bundle_started(
            bundle,
            mode,
            prepare_started,
            Some(&self.subject),
            Some(BatchMetricsKind::EvmoneAdvanced),
        )
    }

    pub fn subject_vm_create_count(&self) -> u64 {
        self.subject.factory.vm_create_count()
    }
}

impl Drop for ReplayEvmoneBatchSession {
    fn drop(&mut self) {
        execution_metrics::disable();
    }
}

impl Drop for ReplayBatchSession {
    fn drop(&mut self) {
        execution_metrics::disable();
    }
}

/// Reference-only batch session. It keeps the process alive across passes but
/// loads no EVMC subject, so process-scoped resource counters describe REVM.
pub struct ReplayReferenceBatchSession;

impl ReplayReferenceBatchSession {
    pub fn new() -> Result<Self, ReplayError> {
        if execution_metrics::is_enabled() {
            return Err(ReplayError::HotBatchSetup(
                "run_exec_loop collection is already enabled on this thread".to_string(),
            ));
        }
        execution_metrics::enable();
        Ok(Self)
    }

    pub fn replay_json(&self, json: &[u8]) -> Result<ReplayReport, ReplayError> {
        let prepare_started = Instant::now();
        let bundle = serde_json::from_slice(json)
            .map_err(|error| ReplayError::InvalidBundleJson(error.to_string()))?;
        replay_bundle_started(
            bundle,
            ReplayMode::ReferenceOnly,
            prepare_started,
            None,
            None,
        )
    }
}

impl Drop for ReplayReferenceBatchSession {
    fn drop(&mut self) {
        execution_metrics::disable();
    }
}

pub fn replay_bundle_json(json: &[u8]) -> Result<ReplayReport, ReplayError> {
    replay_bundle_json_with_mode(json, ReplayMode::Differential)
}

pub fn replay_bundle_json_with_mode(
    json: &[u8],
    mode: ReplayMode,
) -> Result<ReplayReport, ReplayError> {
    let prepare_started = Instant::now();
    let bundle = serde_json::from_slice(json)
        .map_err(|error| ReplayError::InvalidBundleJson(error.to_string()))?;
    replay_bundle_started(bundle, mode, prepare_started, None, None)
}

pub fn replay_bundle(bundle: WitnessBundle) -> Result<ReplayReport, ReplayError> {
    replay_bundle_with_mode(bundle, ReplayMode::Differential)
}

pub fn replay_bundle_with_mode(
    bundle: WitnessBundle,
    mode: ReplayMode,
) -> Result<ReplayReport, ReplayError> {
    replay_bundle_started(bundle, mode, Instant::now(), None, None)
}

fn replay_bundle_started(
    bundle: WitnessBundle,
    mode: ReplayMode,
    prepare_started: Instant,
    shared_subject: Option<&ReplaySubject>,
    batch_metrics: Option<BatchMetricsKind>,
) -> Result<ReplayReport, ReplayError> {
    if bundle.target_block.is_none() {
        return Err(ReplayError::MissingTargetBlock);
    }
    let (reference_db, subject_db) = match mode {
        ReplayMode::Differential => (
            Some(
                WitnessDb::from_bundle(bundle.clone())
                    .map_err(ReplayError::ReferenceWitnessImport)?,
            ),
            Some(WitnessDb::from_bundle(bundle).map_err(ReplayError::SubjectWitnessImport)?),
        ),
        ReplayMode::ReferenceOnly => (
            Some(WitnessDb::from_bundle(bundle).map_err(ReplayError::ReferenceWitnessImport)?),
            None,
        ),
        ReplayMode::SubjectOnly => (
            None,
            Some(WitnessDb::from_bundle(bundle).map_err(ReplayError::SubjectWitnessImport)?),
        ),
    };
    let primary_db = reference_db
        .as_ref()
        .or(subject_db.as_ref())
        .expect("every replay mode executes at least one engine");
    let raw = primary_db
        .target_block()
        .cloned()
        .ok_or(ReplayError::MissingTargetBlock)?;
    let expected_hash = primary_db.target_header().hash_slow();
    let expected_number = primary_db.target_header().number;
    let pre_state_root = primary_db.pre_state_root();
    let parent_header = SealedHeader::seal_slow(primary_db.parent_header().clone());

    let mut input = raw.as_ref();
    let sealed = Block::decode_sealed(&mut input)
        .map_err(|error| ReplayError::RawBlockDecode(error.to_string()))?;
    if !input.is_empty() {
        return Err(ReplayError::RawBlockTrailingBytes);
    }
    let sealed: SealedBlock<Block> = sealed.into();

    let recovered =
        RecoveredBlock::try_recover_sealed(sealed).map_err(|_| ReplayError::SenderRecovery)?;
    let transaction_hashes = recovered
        .body()
        .transactions
        .iter()
        .map(|transaction| *transaction.tx_hash())
        .collect::<Vec<_>>();
    let withdrawal_balance_accounts = recovered
        .body()
        .withdrawals
        .as_ref()
        .into_iter()
        .flatten()
        .map(|withdrawal| withdrawal.address)
        .collect::<BTreeSet<_>>();
    let header = recovered.header();
    let spec = revm_spec(MAINNET.as_ref(), header);
    if spec != SpecId::OSAKA {
        return Err(ReplayError::UnsupportedSpec { actual: spec });
    }
    if header.block_access_list_hash.is_some() {
        return Err(ReplayError::BlockAccessListUnsupported);
    }
    if header.slot_number.is_some() {
        return Err(ReplayError::SlotNumberUnsupported);
    }

    let consensus = EthBeaconConsensus::new(MAINNET.clone());
    consensus
        .validate_header(recovered.sealed_block().sealed_header())
        .map_err(|error| ReplayError::HeaderPreflight(error.to_string()))?;
    consensus
        .validate_header_against_parent(recovered.sealed_block().sealed_header(), &parent_header)
        .map_err(|error| ReplayError::HeaderAgainstParent(error.to_string()))?;
    consensus
        .validate_block_pre_execution(recovered.sealed_block())
        .map_err(|error| ReplayError::BlockPreflight(error.to_string()))?;

    let owned_subject = if mode.runs_subject() && shared_subject.is_none() {
        Some(ReplaySubject::from_env()?)
    } else {
        None
    };
    let replay_subject = if mode.runs_subject() {
        Some(
            shared_subject
                .or(owned_subject.as_ref())
                .expect("subject replay has an owned or shared subject"),
        )
    } else {
        None
    };
    let execution_started = Instant::now();

    let mut reth_revm_execute = None;
    let mut reth_revm_run_exec_loop = None;
    let reference_execution = if let Some(reference_db) = reference_db {
        let started = Instant::now();
        let reference_config =
            EthEvmConfig::new_with_evm_factory(MAINNET.clone(), EthEvmFactory::default());
        let mut reference_executor = BasicBlockExecutor::new(reference_config, reference_db);
        let run_exec_loop_before = run_exec_loop_snapshot();
        let reference_result = match reference_executor.execute_one(&recovered) {
            Ok(result) => result,
            Err(error) => {
                let state = reference_executor.into_state();
                return Err(classify_reference_execution(
                    &error,
                    state.database.strict_db().accesses().len(),
                ));
            }
        };
        reth_revm_run_exec_loop = run_exec_loop_delta(run_exec_loop_before)?;
        let reference_state = reference_executor.into_state();
        let reference_accesses = reference_state.database.strict_db().accesses().to_vec();
        let reference_bundle = reference_state.bundle_state;
        let reference_db = reference_state.database;
        reth_revm_execute = Some(started.elapsed().as_nanos() as u64);
        Some((
            reference_result,
            reference_accesses,
            reference_bundle,
            reference_db,
        ))
    } else {
        None
    };

    let mut reth_subject_execute = None;
    let mut subject_backend_name = None;
    let mut subject_vm_create_count = None;
    let mut dtvm_phase_metrics = None;
    let mut dtvm_hot_metrics = None;
    let mut evmone_advanced_metrics = None;
    let mut reth_subject_run_exec_loop = None;
    let subject_execution = if let Some(subject_db) = subject_db {
        let subject = replay_subject
            .expect("subject replay mode verifies the subject library before execution");
        subject_backend_name = Some(subject.backend.as_str().to_string());
        let started = Instant::now();
        let hot_before = matches!(batch_metrics, Some(BatchMetricsKind::DtvmHot))
            .then(|| subject.factory.hot_metrics_snapshot())
            .transpose()
            .map_err(ReplayError::HotBatchSetup)?;
        let evmone_before = matches!(batch_metrics, Some(BatchMetricsKind::EvmoneAdvanced))
            .then(|| subject.factory.evmone_diagnostic_metrics_snapshot())
            .transpose()
            .map_err(ReplayError::HotBatchSetup)?;
        let subject_config =
            EthEvmConfig::new_with_evm_factory(MAINNET.clone(), subject.factory.clone());
        let mut subject_executor = BasicBlockExecutor::new(subject_config, subject_db);
        let run_exec_loop_before = run_exec_loop_snapshot();
        let subject_result = match subject_executor.execute_one(&recovered) {
            Ok(result) => result,
            Err(error) => {
                let state = subject_executor.into_state();
                return Err(classify_subject_execution(
                    &error,
                    state.database.strict_db().accesses().len(),
                    &transaction_hashes,
                ));
            }
        };
        reth_subject_run_exec_loop = run_exec_loop_delta(run_exec_loop_before)?;
        if let Some(before) = hot_before {
            let after = subject
                .factory
                .hot_metrics_snapshot()
                .map_err(ReplayError::HotBatchSetup)?;
            dtvm_hot_metrics = Some(DtvmHotMetricsObservation::checked(before, after)?);
        }
        if let Some(before) = evmone_before {
            let after = subject
                .factory
                .evmone_diagnostic_metrics_snapshot()
                .map_err(ReplayError::HotBatchSetup)?;
            evmone_advanced_metrics =
                Some(EvmoneAdvancedMetricsObservation::checked(before, after)?);
        }
        let subject_state = subject_executor.into_state();
        subject_vm_create_count = Some(subject.factory.vm_create_count());
        // A production batch shares the subject but still projects the stable
        // disabled metrics-ABI status into every record. Diagnostic batches
        // keep using their explicit before/after observation instead.
        if should_project_dtvm_phase_metrics(shared_subject.is_some(), batch_metrics) {
            dtvm_phase_metrics = subject.factory.dtvm_phase_metrics().map(Into::into);
        }
        let subject_accesses = subject_state.database.strict_db().accesses().to_vec();
        let subject_bundle = subject_state.bundle_state;
        let subject_db = subject_state.database;
        reth_subject_execute = Some(started.elapsed().as_nanos() as u64);
        Some((subject_result, subject_accesses, subject_bundle, subject_db))
    } else {
        None
    };
    let validation_roots_started = Instant::now();

    if let Some((reference_result, _, _, _)) = &reference_execution {
        validate_block_post_execution(&recovered, MAINNET.as_ref(), reference_result, None, None)
            .map_err(|error| ReplayError::ReferencePostValidation(error.to_string()))?;
    }
    if let Some((subject_result, _, _, _)) = &subject_execution {
        validate_block_post_execution(&recovered, MAINNET.as_ref(), subject_result, None, None)
            .map_err(|error| ReplayError::SubjectPostValidation(error.to_string()))?;
    }

    let expected_blob_gas = header.blob_gas_used.unwrap_or(0);
    if let Some((reference_result, _, _, _)) = &reference_execution {
        if reference_result.blob_gas_used != expected_blob_gas {
            return Err(ReplayError::ReferenceBlobGasMismatch {
                expected: expected_blob_gas,
                actual: reference_result.blob_gas_used,
            });
        }
    }
    if let Some((subject_result, _, _, _)) = &subject_execution {
        if subject_result.blob_gas_used != expected_blob_gas {
            return Err(ReplayError::SubjectBlobGasMismatch {
                expected: expected_blob_gas,
                actual: subject_result.blob_gas_used,
            });
        }
    }

    if mode == ReplayMode::Differential {
        let (reference_result, reference_accesses, reference_bundle, _) = reference_execution
            .as_ref()
            .expect("differential mode runs the reference");
        let (subject_result, subject_accesses, subject_bundle, _) = subject_execution
            .as_ref()
            .expect("differential mode runs the subject");
        if subject_result != reference_result {
            return Err(ReplayError::ExecutionResultMismatch);
        }
        if !access_sequences_eq_with_withdrawal_tail(
            subject_accesses,
            reference_accesses,
            &withdrawal_balance_accounts,
        ) {
            let index = subject_accesses
                .iter()
                .zip(reference_accesses)
                .position(|(subject, reference)| subject != reference)
                .unwrap_or_else(|| subject_accesses.len().min(reference_accesses.len()));
            return Err(ReplayError::AccessSequenceMismatch {
                index,
                reference: reference_accesses
                    .get(index..)
                    .map_or_else(|| "<end>".to_string(), |accesses| format!("{accesses:?}")),
                subject: subject_accesses
                    .get(index..)
                    .map_or_else(|| "<end>".to_string(), |accesses| format!("{accesses:?}")),
                reference_len: reference_accesses.len(),
                subject_len: subject_accesses.len(),
            });
        }
        if !bundle_state_semantics_eq(subject_bundle, reference_bundle) {
            return Err(ReplayError::BundleStateMismatch);
        }
    }

    let (receipt_count, gas_used, blob_gas_used) = reference_execution
        .as_ref()
        .map(|(result, _, _, _)| result)
        .or_else(|| subject_execution.as_ref().map(|(result, _, _, _)| result))
        .map(|result| (result.receipts.len(), result.gas_used, result.blob_gas_used))
        .expect("every replay mode produces one execution result");

    let reference_post_state_root = reference_execution
        .map(|(_, _, bundle, db)| {
            db.into_verified_post_state_root(&bundle)
                .map_err(ReplayError::ReferencePostStateRoot)
        })
        .transpose()?;
    let subject_post_state_root = subject_execution
        .map(|(_, _, bundle, db)| {
            db.into_verified_post_state_root(&bundle)
                .map_err(ReplayError::SubjectPostStateRoot)
        })
        .transpose()?;
    let post_state_root = reference_post_state_root
        .or(subject_post_state_root)
        .expect("every replay mode verifies one post-state root");
    let validation_roots_finished = Instant::now();

    Ok(ReplayReport {
        replay_mode: mode,
        subject_backend: subject_backend_name,
        subject_vm_create_count,
        dtvm_phase_metrics,
        dtvm_hot_metrics,
        evmone_advanced_metrics,
        reth_revm_run_exec_loop,
        reth_subject_run_exec_loop,
        differential_match: (mode == ReplayMode::Differential).then_some(true),
        raw_bound: true,
        pre_execution_commitments: true,
        post_execution_commitments: PostExecutionCommitments {
            gas_used: true,
            receipts_root: true,
            logs_bloom: true,
            requests_hash: true,
            blob_gas_used: true,
        },
        pre_state_root,
        pre_state_root_verified: true,
        post_state_root,
        post_state_root_verified: true,
        block_number: expected_number,
        block_hash: expected_hash,
        raw_block_bytes: raw.len(),
        transaction_count: recovered.senders().len(),
        receipt_count,
        gas_used,
        blob_gas_used,
        phase_wall_time_ns: ReplayPhaseWallTimeNs {
            prepare_import_preflight: execution_started.duration_since(prepare_started).as_nanos()
                as u64,
            reth_revm_execute,
            reth_subject_execute,
            validation_roots: validation_roots_finished
                .duration_since(validation_roots_started)
                .as_nanos() as u64,
        },
    })
}

fn should_project_dtvm_phase_metrics(
    shared_subject: bool,
    batch_metrics: Option<BatchMetricsKind>,
) -> bool {
    !shared_subject || batch_metrics.is_none()
}

impl From<AdapterDtvmPhaseMetricsReport> for DtvmPhaseMetricsReport {
    fn from(report: AdapterDtvmPhaseMetricsReport) -> Self {
        Self {
            status: report.status.as_str().to_string(),
            status_code: report.status.code(),
            metrics: report.metrics.map(Into::into),
        }
    }
}

impl From<AdapterDtvmEvmcPhaseMetrics> for DtvmEvmcPhaseMetrics {
    fn from(metrics: AdapterDtvmEvmcPhaseMetrics) -> Self {
        Self {
            version: metrics.version,
            struct_size: metrics.struct_size,
            top_level_execute_count: metrics.top_level_execute_count,
            top_level_execute_wall_ns: metrics.top_level_execute_wall_ns,
            synchronous_jit_compile_attempt_count: metrics.synchronous_jit_compile_attempt_count,
            synchronous_jit_compile_success_count: metrics.synchronous_jit_compile_success_count,
            synchronous_jit_compile_wall_ns: metrics.synchronous_jit_compile_wall_ns,
            non_compile_residual_ns: metrics.non_compile_residual_ns,
            profile_guided_jit_trigger_count: metrics.profile_guided_jit_trigger_count,
            module_cache_lookup_count: metrics.module_cache_lookup_count,
            module_cache_hit_count: metrics.module_cache_hit_count,
            module_cache_miss_count: metrics.module_cache_miss_count,
            module_cache_validation_reject_count: metrics.module_cache_validation_reject_count,
            module_cache_eviction_count: metrics.module_cache_eviction_count,
            module_cache_entry_count: metrics.module_cache_entry_count,
            module_cache_peak_entry_count: metrics.module_cache_peak_entry_count,
            transient_module_load_count: metrics.transient_module_load_count,
            jit_frame_count: metrics.jit_frame_count,
            jit_active_wall_ns: metrics.jit_active_wall_ns,
            interpreter_frame_count: metrics.interpreter_frame_count,
            interpreter_active_wall_ns: metrics.interpreter_active_wall_ns,
            create_interpreter_fallback_count: metrics.create_interpreter_fallback_count,
            newly_created_interpreter_fallback_count: metrics
                .newly_created_interpreter_fallback_count,
            small_code_interpreter_fallback_count: metrics.small_code_interpreter_fallback_count,
            sticky_interpreter_fallback_count: metrics.sticky_interpreter_fallback_count,
        }
    }
}

impl From<AdapterRunExecLoopMetrics> for RunExecLoopMetrics {
    fn from(metrics: AdapterRunExecLoopMetrics) -> Self {
        Self {
            call_count: metrics.call_count,
            wall_ns: metrics.wall_ns,
        }
    }
}

impl From<AdapterDtvmEvmcHotMetrics> for DtvmHotMetricsSnapshot {
    fn from(metrics: AdapterDtvmEvmcHotMetrics) -> Self {
        Self {
            version: metrics.version,
            struct_size: metrics.struct_size,
            top_level_execute_count: metrics.top_level_execute_count,
            top_level_execute_wall_ns: metrics.top_level_execute_wall_ns,
            synchronous_jit_compile_attempt_count: metrics.synchronous_jit_compile_attempt_count,
            synchronous_jit_compile_success_count: metrics.synchronous_jit_compile_success_count,
            synchronous_jit_compile_wall_ns: metrics.synchronous_jit_compile_wall_ns,
            non_compile_residual_ns: metrics.non_compile_residual_ns,
            profile_guided_jit_trigger_count: metrics.profile_guided_jit_trigger_count,
            module_cache_lookup_count: metrics.module_cache_lookup_count,
            module_cache_hit_count: metrics.module_cache_hit_count,
            module_cache_miss_count: metrics.module_cache_miss_count,
            module_cache_validation_reject_count: metrics.module_cache_validation_reject_count,
            module_cache_eviction_count: metrics.module_cache_eviction_count,
            module_cache_entry_count: metrics.module_cache_entry_count,
            module_cache_peak_entry_count: metrics.module_cache_peak_entry_count,
            transient_module_load_count: metrics.transient_module_load_count,
            jit_frame_count: metrics.jit_frame_count,
            jit_active_wall_ns: metrics.jit_active_wall_ns,
            interpreter_frame_count: metrics.interpreter_frame_count,
            interpreter_active_wall_ns: metrics.interpreter_active_wall_ns,
            create_interpreter_fallback_count: metrics.create_interpreter_fallback_count,
            newly_created_interpreter_fallback_count: metrics
                .newly_created_interpreter_fallback_count,
            small_code_interpreter_fallback_count: metrics.small_code_interpreter_fallback_count,
            sticky_interpreter_fallback_count: metrics.sticky_interpreter_fallback_count,
        }
    }
}

impl DtvmHotMetricsObservation {
    fn checked(
        before: AdapterDtvmEvmcHotMetrics,
        after: AdapterDtvmEvmcHotMetrics,
    ) -> Result<Self, ReplayError> {
        macro_rules! delta {
            ($field:ident) => {
                after
                    .$field
                    .checked_sub(before.$field)
                    .ok_or(ReplayError::HotMetricsNonMonotonic(stringify!($field)))?
            };
        }
        let delta = DtvmHotMetricsDelta {
            top_level_execute_count: delta!(top_level_execute_count),
            top_level_execute_wall_ns: delta!(top_level_execute_wall_ns),
            synchronous_jit_compile_attempt_count: delta!(synchronous_jit_compile_attempt_count),
            synchronous_jit_compile_success_count: delta!(synchronous_jit_compile_success_count),
            synchronous_jit_compile_wall_ns: delta!(synchronous_jit_compile_wall_ns),
            non_compile_residual_ns: delta!(non_compile_residual_ns),
            profile_guided_jit_trigger_count: delta!(profile_guided_jit_trigger_count),
            module_cache_lookup_count: delta!(module_cache_lookup_count),
            module_cache_hit_count: delta!(module_cache_hit_count),
            module_cache_miss_count: delta!(module_cache_miss_count),
            module_cache_validation_reject_count: delta!(module_cache_validation_reject_count),
            module_cache_eviction_count: delta!(module_cache_eviction_count),
            module_cache_entry_count_before: before.module_cache_entry_count,
            module_cache_entry_count_after: after.module_cache_entry_count,
            module_cache_peak_entry_count: delta!(module_cache_peak_entry_count),
            transient_module_load_count: delta!(transient_module_load_count),
            jit_frame_count: delta!(jit_frame_count),
            jit_active_wall_ns: delta!(jit_active_wall_ns),
            interpreter_frame_count: delta!(interpreter_frame_count),
            interpreter_active_wall_ns: delta!(interpreter_active_wall_ns),
            create_interpreter_fallback_count: delta!(create_interpreter_fallback_count),
            newly_created_interpreter_fallback_count: delta!(
                newly_created_interpreter_fallback_count
            ),
            small_code_interpreter_fallback_count: delta!(small_code_interpreter_fallback_count),
            sticky_interpreter_fallback_count: delta!(sticky_interpreter_fallback_count),
        };
        Ok(Self {
            before: before.into(),
            after: after.into(),
            delta,
        })
    }
}

impl From<AdapterEvmoneAdvancedDiagnosticMetrics> for EvmoneAdvancedMetricsSnapshot {
    fn from(metrics: AdapterEvmoneAdvancedDiagnosticMetrics) -> Self {
        Self {
            version: metrics.version,
            struct_size: metrics.struct_size,
            top_level_execute_count: metrics.top_level_execute_count,
            top_level_execute_wall_ns: metrics.top_level_execute_wall_ns,
            advanced_analysis_count: metrics.advanced_analysis_count,
            advanced_analysis_wall_ns: metrics.advanced_analysis_wall_ns,
            advanced_state_setup_count: metrics.advanced_state_setup_count,
            advanced_state_setup_wall_ns: metrics.advanced_state_setup_wall_ns,
            advanced_core_execute_count: metrics.advanced_core_execute_count,
            advanced_core_execute_wall_ns: metrics.advanced_core_execute_wall_ns,
        }
    }
}

impl EvmoneAdvancedMetricsObservation {
    fn checked(
        before: AdapterEvmoneAdvancedDiagnosticMetrics,
        after: AdapterEvmoneAdvancedDiagnosticMetrics,
    ) -> Result<Self, ReplayError> {
        macro_rules! delta {
            ($field:ident) => {
                after
                    .$field
                    .checked_sub(before.$field)
                    .ok_or(ReplayError::EvmoneMetricsNonMonotonic(stringify!($field)))?
            };
        }
        Ok(Self {
            before: before.into(),
            after: after.into(),
            delta: EvmoneAdvancedMetricsDelta {
                top_level_execute_count: delta!(top_level_execute_count),
                top_level_execute_wall_ns: delta!(top_level_execute_wall_ns),
                advanced_analysis_count: delta!(advanced_analysis_count),
                advanced_analysis_wall_ns: delta!(advanced_analysis_wall_ns),
                advanced_state_setup_count: delta!(advanced_state_setup_count),
                advanced_state_setup_wall_ns: delta!(advanced_state_setup_wall_ns),
                advanced_core_execute_count: delta!(advanced_core_execute_count),
                advanced_core_execute_wall_ns: delta!(advanced_core_execute_wall_ns),
            },
        })
    }
}

fn run_exec_loop_snapshot() -> Option<AdapterRunExecLoopMetrics> {
    execution_metrics::is_enabled().then(execution_metrics::snapshot)
}

fn run_exec_loop_delta(
    before: Option<AdapterRunExecLoopMetrics>,
) -> Result<Option<RunExecLoopMetrics>, ReplayError> {
    before
        .map(|before| {
            execution_metrics::snapshot()
                .checked_delta(before)
                .map(Into::into)
                .ok_or(ReplayError::RunExecLoopMetricsNonMonotonic)
        })
        .transpose()
}

fn bundle_state_semantics_eq(actual: &BundleState, expected: &BundleState) -> bool {
    actual.state == expected.state
        && actual.contracts == expected.contracts
        && actual.reverts.content_eq(&expected.reverts)
}

fn access_sequences_eq_with_withdrawal_tail(
    actual: &[DbAccess],
    expected: &[DbAccess],
    withdrawal_accounts: &BTreeSet<Address>,
) -> bool {
    if actual == expected {
        return true;
    }
    if withdrawal_accounts.is_empty() || actual.len() != expected.len() {
        return false;
    }

    let common_prefix = actual
        .iter()
        .zip(expected)
        .take_while(|(actual, expected)| actual == expected)
        .count();
    let mut expected_tail_accounts = withdrawal_accounts.clone();
    for access in &actual[..common_prefix] {
        if let DbAccess::Basic(address) = access {
            expected_tail_accounts.remove(address);
        }
    }
    let actual_tail = basic_accounts(&actual[common_prefix..]);
    let expected_tail = basic_accounts(&expected[common_prefix..]);

    matches!(
        (actual_tail, expected_tail),
        (Some(actual), Some(expected))
            if !expected_tail_accounts.is_empty()
                && actual == expected_tail_accounts
                && expected == expected_tail_accounts
    )
}

fn basic_accounts(accesses: &[DbAccess]) -> Option<BTreeSet<Address>> {
    let mut accounts = BTreeSet::new();
    for access in accesses {
        let DbAccess::Basic(address) = access else {
            return None;
        };
        if !accounts.insert(*address) {
            return None;
        }
    }
    Some(accounts)
}

fn classify_reference_execution(error: &(dyn Error + 'static), access_count: usize) -> ReplayError {
    let message = error.to_string();
    if error_chain_contains_witness(error) || message_contains_witness_error(&message) {
        ReplayError::ReferenceWitnessIncomplete {
            message,
            access_count,
        }
    } else {
        ReplayError::ReferenceExecution {
            message,
            access_count,
        }
    }
}

fn classify_subject_execution(
    error: &(dyn Error + 'static),
    access_count: usize,
    transaction_hashes: &[B256],
) -> ReplayError {
    let message = error.to_string();
    if error_chain_contains_witness(error) || message_contains_witness_error(&message) {
        ReplayError::SubjectUnprovenExtraAccess {
            message,
            access_count,
        }
    } else if let Some((transaction_index, reason)) =
        adapter_unsupported(&message, transaction_hashes)
    {
        ReplayError::AdapterUnsupported {
            transaction_index,
            reason,
        }
    } else {
        ReplayError::SubjectExecution {
            message,
            access_count,
        }
    }
}

fn error_chain_contains_witness(mut error: &(dyn Error + 'static)) -> bool {
    loop {
        if error.downcast_ref::<WitnessImportError>().is_some() {
            return true;
        }
        let Some(source) = error.source() else {
            return false;
        };
        error = source;
    }
}

fn message_contains_witness_error(message: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "parent state root ",
        "failed to reconstruct witness multiproof: ",
        "failed to reveal witness multiproof: ",
        "revealed pre-state root mismatch: ",
        "account proof is incomplete for ",
        "account leaf for ",
        "storage manifest names an account that was not resolved: ",
        "storage proof is incomplete for ",
        "storage leaf for ",
        "account is outside the proven witness: ",
        "code is outside the proven witness: ",
        "code witness hash mismatch: ",
        "storage is outside the proven witness: ",
        "an absent account cannot have nonzero proven storage: ",
        "block hash is outside the proven witness: ",
        "account id ",
    ];

    PREFIXES
        .iter()
        .any(|prefix| message_has_prefixed_segment(message, prefix))
}

fn message_has_prefixed_segment(message: &str, prefix: &str) -> bool {
    message.starts_with(prefix)
        || message
            .match_indices(": ")
            .any(|(index, _)| message[index + 2..].starts_with(prefix))
}

fn adapter_unsupported(
    message: &str,
    transaction_hashes: &[B256],
) -> Option<(usize, &'static str)> {
    const PREFIX: &str = "internal EVM error occurred when executing transaction ";
    const EXACT_REASONS: &[(&str, &str)] = &[
        (
            "nested frame reached the EVMC subject first slice",
            "nested frame reached the EVMC subject first slice",
        ),
        (
            "EIP-8037 reservoir is unsupported",
            "EIP-8037 reservoir is unsupported",
        ),
        (
            "CREATE/empty frame is unsupported",
            "CREATE/empty frame is unsupported",
        ),
        (
            "only a non-static, non-delegated ordinary CALL is supported",
            "only a non-static, non-delegated ordinary CALL is supported",
        ),
        (
            "nested CALL code address differs from its recipient",
            "nested CALL code address differs from its recipient",
        ),
        (
            "SELFDESTRUCT is unsupported until it is bridged to the REVM journal",
            "SELFDESTRUCT is unsupported until it is bridged to the REVM journal",
        ),
        (
            "nested EIP-8037 reservoir is unsupported",
            "nested EIP-8037 reservoir is unsupported",
        ),
    ];

    let (hash, details) = message.strip_prefix(PREFIX)?.split_once(": ")?;
    let transaction_index = transaction_hashes
        .iter()
        .position(|transaction_hash| transaction_hash.to_string() == hash)?;
    if let Some((_, canonical)) = EXACT_REASONS
        .iter()
        .find(|(reason, _)| message_has_exact_segment(details, reason))
    {
        return Some((transaction_index, canonical));
    }
    for (candidate, reason) in [
        (
            "nested call kind is unsupported (kind=3)",
            "nested CREATE is unsupported (kind=3)",
        ),
        (
            "nested call kind is unsupported (kind=4)",
            "nested CREATE2 is unsupported (kind=4)",
        ),
        (
            "nested call kind is unsupported (kind=5)",
            "nested EOFCREATE is unsupported (kind=5)",
        ),
    ] {
        if message_has_exact_segment(details, candidate) {
            return Some((transaction_index, reason));
        }
    }
    if message_has_dynamic_suffix_segment(details, "nested call kind is unsupported (kind=", false)
    {
        return Some((transaction_index, "nested call kind is unsupported"));
    }
    if message_has_dynamic_suffix_segment(
        details,
        "nested CALL flags are unsupported (flags=0x",
        true,
    ) {
        return Some((transaction_index, "nested CALL flags are unsupported"));
    }
    None
}

fn message_has_exact_segment(message: &str, expected: &str) -> bool {
    message == expected
        || message
            .match_indices(": ")
            .any(|(index, _)| &message[index + 2..] == expected)
}

fn message_has_dynamic_suffix_segment(message: &str, prefix: &str, hexadecimal: bool) -> bool {
    error_segments(message).any(|segment| {
        let Some(value) = segment
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(')'))
        else {
            return false;
        };
        !value.is_empty()
            && if hexadecimal {
                value.bytes().all(|byte| byte.is_ascii_hexdigit())
            } else {
                value.parse::<i32>().is_ok()
            }
    })
}

fn error_segments(message: &str) -> impl Iterator<Item = &str> {
    std::iter::once(message).chain(
        message
            .match_indices(": ")
            .map(|(index, _)| &message[index + 2..]),
    )
}

struct VerifiedSubjectLibrary {
    _file: File,
    path: PathBuf,
}

impl VerifiedSubjectLibrary {
    fn path(&self) -> &Path {
        &self.path
    }
}

struct ReplaySubject {
    backend: SubjectBackend,
    _library: VerifiedSubjectLibrary,
    factory: SubjectEvmFactory,
}

impl ReplaySubject {
    fn from_env() -> Result<Self, ReplayError> {
        let (backend, library) = verified_subject_library_from_env()?;
        let factory = SubjectEvmFactory::new_for(library.path(), backend);
        Ok(Self {
            backend,
            _library: library,
            factory,
        })
    }
}

fn verified_subject_library_from_env(
) -> Result<(SubjectBackend, VerifiedSubjectLibrary), ReplayError> {
    let backend = SubjectBackend::from_env()
        .map_err(|error| ReplayError::SubjectProvenance(error.to_string()))?;
    if matches!(
        backend,
        SubjectBackend::DtvmEager | SubjectBackend::DtvmProfileGuided
    ) {
        match std::env::var("DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION") {
            Ok(value) if value == "true" => {}
            Ok(_) => {
                return Err(ReplayError::SubjectProvenance(
                    "DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION must equal true for \
                     DTVM subjects"
                        .to_string(),
                ));
            }
            Err(error) => {
                return Err(ReplayError::SubjectProvenance(format!(
                    "DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION is required for \
                     DTVM subjects: {error}"
                )));
            }
        }
    }

    let path = std::env::var_os("RETH_SUBJECT_LIBRARY")
        .map(PathBuf::from)
        .ok_or_else(|| {
            ReplayError::SubjectProvenance("RETH_SUBJECT_LIBRARY is required".to_string())
        })?;
    let expected = std::env::var("RETH_SUBJECT_LIBRARY_SHA256").map_err(|error| {
        ReplayError::SubjectProvenance(format!("RETH_SUBJECT_LIBRARY_SHA256 is required: {error}"))
    })?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ReplayError::SubjectProvenance(
            "RETH_SUBJECT_LIBRARY_SHA256 must be exactly 64 hexadecimal characters".to_string(),
        ));
    }
    let library = seal_verified_subject_library(&path, &expected)?;
    Ok((backend, library))
}

fn seal_verified_subject_library(
    source_path: &Path,
    expected_sha256: &str,
) -> Result<VerifiedSubjectLibrary, ReplayError> {
    let mut source = File::open(source_path).map_err(|error| {
        ReplayError::SubjectProvenance(format!("failed to open RETH_SUBJECT_LIBRARY: {error}"))
    })?;
    let name = CString::new("reth-subject-verified-library")
        .expect("static memfd name contains no interior NUL");
    // SAFETY: `name` is NUL-terminated and the flags are valid Linux memfd flags.
    let fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(ReplayError::SubjectProvenance(format!(
            "failed to create sealed subject memfd: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `memfd_create` returned a new owned file descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(|error| {
            ReplayError::SubjectProvenance(format!("failed to read RETH_SUBJECT_LIBRARY: {error}"))
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        file.write_all(&buffer[..count]).map_err(|error| {
            ReplayError::SubjectProvenance(format!(
                "failed to copy RETH_SUBJECT_LIBRARY to memfd: {error}"
            ))
        })?;
    }
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(ReplayError::SubjectProvenance(format!(
            "subject library SHA-256 mismatch: expected {expected_sha256}, got {actual}"
        )));
    }

    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    // SAFETY: `file` owns a valid memfd descriptor and `seals` contains only F_SEAL_* flags.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(ReplayError::SubjectProvenance(format!(
            "failed to seal subject memfd: {}",
            std::io::Error::last_os_error()
        )));
    }
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    Ok(VerifiedSubjectLibrary { _file: file, path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccessManifest;
    use alloy_consensus::{Header, SignableTransaction, TxEip7702, TxLegacy};
    use alloy_eips::{
        eip2930::AccessList,
        eip7685::EMPTY_REQUESTS_HASH,
        eip7702::{Authorization, SignedAuthorization},
    };
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use alloy_rpc_types_debug::ExecutionWitness;
    use alloy_trie::EMPTY_ROOT_HASH;
    use reth_ethereum_primitives::{Transaction, TransactionSigned};

    const TARGET_NUMBER: u64 = 24_000_000;
    const TARGET_TIMESTAMP: u64 = 1_800_000_000;

    #[test]
    fn shared_production_projects_disabled_phase_metrics_but_diagnostic_does_not() {
        assert!(should_project_dtvm_phase_metrics(true, None));
        assert!(!should_project_dtvm_phase_metrics(
            true,
            Some(BatchMetricsKind::DtvmHot)
        ));
        assert!(!should_project_dtvm_phase_metrics(
            true,
            Some(BatchMetricsKind::EvmoneAdvanced)
        ));
        assert!(should_project_dtvm_phase_metrics(false, None));
    }

    #[test]
    fn withdrawal_tail_order_does_not_change_access_semantics() {
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let prefix = DbAccess::Code(B256::repeat_byte(0x33));
        let actual = [
            prefix.clone(),
            DbAccess::Basic(first),
            DbAccess::Basic(second),
        ];
        let expected = [prefix, DbAccess::Basic(second), DbAccess::Basic(first)];

        assert!(access_sequences_eq_with_withdrawal_tail(
            &actual,
            &expected,
            &BTreeSet::from([first, second]),
        ));
    }

    #[test]
    fn withdrawal_account_loaded_in_prefix_is_not_required_in_tail() {
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let third = Address::repeat_byte(0x33);
        let actual = [
            DbAccess::Basic(first),
            DbAccess::Basic(second),
            DbAccess::Basic(third),
        ];
        let expected = [
            DbAccess::Basic(first),
            DbAccess::Basic(third),
            DbAccess::Basic(second),
        ];

        assert!(access_sequences_eq_with_withdrawal_tail(
            &actual,
            &expected,
            &BTreeSet::from([first, second, third]),
        ));
    }

    #[test]
    fn withdrawal_tail_comparison_rejects_a_different_account() {
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let unexpected = Address::repeat_byte(0x44);
        let actual = [DbAccess::Basic(first), DbAccess::Basic(unexpected)];
        let expected = [DbAccess::Basic(second), DbAccess::Basic(first)];

        assert!(!access_sequences_eq_with_withdrawal_tail(
            &actual,
            &expected,
            &BTreeSet::from([first, second]),
        ));
    }

    #[test]
    fn withdrawal_tail_comparison_rejects_duplicates() {
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let actual = [DbAccess::Basic(first), DbAccess::Basic(first)];
        let expected = [DbAccess::Basic(second), DbAccess::Basic(first)];

        assert!(!access_sequences_eq_with_withdrawal_tail(
            &actual,
            &expected,
            &BTreeSet::from([first, second]),
        ));
    }

    #[test]
    fn withdrawal_tail_comparison_rejects_non_basic_reordering() {
        let account = Address::repeat_byte(0x11);
        let slot = U256::from(7);
        let actual = [DbAccess::Storage(account, slot), DbAccess::Basic(account)];
        let expected = [DbAccess::Basic(account), DbAccess::Storage(account, slot)];

        assert!(!access_sequences_eq_with_withdrawal_tail(
            &actual,
            &expected,
            &BTreeSet::from([account]),
        ));
    }

    /// Covers strict full-block plumbing and empty-code system orchestration.
    ///
    /// The block has no transactions, so this does not claim that the subject VM
    /// executes contract bytecode; that remains covered by the signed CALL
    /// differential integration test.
    #[test]
    fn output_valid_empty_osaka_block_passes_strict_executor_orchestration() {
        let bundle = output_valid_empty_osaka_bundle();
        let report = replay_bundle(bundle).expect("strict full-block differential replay");

        assert_eq!(report.differential_match, Some(true));
        assert!(report.raw_bound);
        assert!(report.pre_execution_commitments);
        assert!(report.post_execution_commitments.gas_used);
        assert!(report.post_execution_commitments.receipts_root);
        assert!(report.post_execution_commitments.logs_bloom);
        assert!(report.post_execution_commitments.requests_hash);
        assert!(report.post_execution_commitments.blob_gas_used);
        assert_eq!(report.pre_state_root, EMPTY_ROOT_HASH);
        assert!(report.pre_state_root_verified);
        assert_eq!(report.post_state_root, EMPTY_ROOT_HASH);
        assert!(report.post_state_root_verified);
        assert_eq!(report.transaction_count, 0);
        assert_eq!(report.receipt_count, 0);
        assert_eq!(report.gas_used, 0);
        assert_eq!(report.blob_gas_used, 0);
    }

    #[test]
    fn legacy_header_only_bundle_is_rejected_before_import() {
        let mut bundle = output_valid_empty_osaka_bundle();
        bundle.target_block = None;
        assert!(matches!(
            replay_bundle(bundle),
            Err(ReplayError::MissingTargetBlock)
        ));
    }

    #[test]
    fn invalid_transaction_signature_is_rejected() {
        let mut bundle = output_valid_empty_osaka_bundle();
        let header = target_block(&bundle).header;
        let transaction = invalid_signed_transaction(TxKind::Call(Address::repeat_byte(0x44)));
        let mut block = Block::from_transactions(header, [transaction]);
        block.body.withdrawals = Some(Default::default());
        block.header.withdrawals_root = block.body.calculate_withdrawals_root();
        bind_target_block(&mut bundle, block);

        assert!(matches!(
            replay_bundle(bundle),
            Err(ReplayError::SenderRecovery)
        ));
    }

    #[test]
    fn invalid_header_fails_consensus_preflight() {
        let mut bundle = output_valid_empty_osaka_bundle();
        let mut block = target_block(&bundle);
        block.header.extra_data = Bytes::from(vec![0u8; 33]);
        bind_target_block(&mut bundle, block);

        assert!(matches!(
            replay_bundle(bundle),
            Err(ReplayError::HeaderPreflight(_))
        ));
    }

    #[test]
    fn base_fee_discontinuity_fails_parent_consensus_preflight() {
        let mut bundle = output_valid_empty_osaka_bundle();
        let mut block = target_block(&bundle);
        block.header.base_fee_per_gas = Some(2);
        bind_target_block(&mut bundle, block);

        assert!(matches!(
            replay_bundle(bundle),
            Err(ReplayError::HeaderAgainstParent(_))
        ));
    }

    #[test]
    fn stringified_system_call_database_error_is_witness_incomplete() {
        let error = OpaqueExecutionError(
            "failed to apply blockhash contract call: database error: \
             account is outside the proven witness: 0x1111111111111111111111111111111111111111"
                .to_string(),
        );

        assert!(matches!(
            classify_reference_execution(&error, 3),
            ReplayError::ReferenceWitnessIncomplete {
                access_count: 3,
                ..
            }
        ));
        assert!(matches!(
            classify_subject_execution(&error, 4, &[]),
            ReplayError::SubjectUnprovenExtraAccess {
                access_count: 4,
                ..
            }
        ));
    }

    #[test]
    fn allowlisted_custom_error_is_adapter_unsupported() {
        let transaction_hash = B256::repeat_byte(0x11);
        for (details, expected_reason) in [
            (
                "EVMC subject execution failed: host callback failed closed: \
                 nested call kind is unsupported (kind=2)",
                "nested call kind is unsupported",
            ),
            (
                "EVMC subject execution failed: host callback failed closed: \
                 nested call kind is unsupported (kind=3)",
                "nested CREATE is unsupported (kind=3)",
            ),
            (
                "EVMC subject execution failed: host callback failed closed: \
                 nested call kind is unsupported (kind=4)",
                "nested CREATE2 is unsupported (kind=4)",
            ),
            (
                "EVMC subject execution failed: host callback failed closed: \
                 nested call kind is unsupported (kind=5)",
                "nested EOFCREATE is unsupported (kind=5)",
            ),
            (
                "EVMC subject execution failed: host callback failed closed: \
                 nested CALL flags are unsupported (flags=0x4)",
                "nested CALL flags are unsupported",
            ),
            (
                "EVMC subject execution failed: host callback failed closed: \
                 nested CALL code address differs from its recipient",
                "nested CALL code address differs from its recipient",
            ),
            (
                "EVMC subject execution failed: host callback failed closed: \
                 SELFDESTRUCT is unsupported until it is bridged to the REVM journal",
                "SELFDESTRUCT is unsupported until it is bridged to the REVM journal",
            ),
            (
                "EVMC subject execution failed: host callback failed closed: \
                 nested EIP-8037 reservoir is unsupported",
                "nested EIP-8037 reservoir is unsupported",
            ),
        ] {
            let error = OpaqueExecutionError(format!(
                "internal EVM error occurred when executing transaction \
                 {transaction_hash}: {details}"
            ));
            match classify_subject_execution(&error, 5, &[transaction_hash]) {
                ReplayError::AdapterUnsupported {
                    transaction_index,
                    reason,
                } => {
                    assert_eq!(transaction_index, 0);
                    assert_eq!(reason, expected_reason);
                }
                other => panic!("expected adapterUnsupported, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_custom_error_remains_subject_execution() {
        let transaction_hash = B256::repeat_byte(0x22);
        let error = OpaqueExecutionError(format!(
            "internal EVM error occurred when executing transaction {transaction_hash}: \
             unknown adapter failure"
        ));

        assert!(matches!(
            classify_subject_execution(&error, 6, &[transaction_hash]),
            ReplayError::SubjectExecution {
                access_count: 6,
                ..
            }
        ));
    }

    #[test]
    fn verified_subject_memfd_is_write_sealed() {
        let contents = b"deterministic subject library test bytes";
        let source_path = temporary_library_source(contents);
        let expected = sha256(contents);

        let mut library =
            seal_verified_subject_library(&source_path, &expected).expect("seal verified library");
        std::fs::remove_file(&source_path).expect("remove temporary library source");
        let error = library
            ._file
            .write_all(b"x")
            .expect_err("F_SEAL_WRITE must reject writes");
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn replacing_source_does_not_change_verified_memfd_bytes() {
        let original = b"verified original subject bytes";
        let source_path = temporary_library_source(original);
        let library = seal_verified_subject_library(&source_path, &sha256(original))
            .expect("seal verified library");

        std::fs::write(&source_path, b"replacement bytes").expect("replace original source");
        assert_eq!(
            std::fs::read(library.path()).expect("read sealed subject memfd"),
            original
        );
        std::fs::remove_file(source_path).expect("remove temporary library source");
    }

    #[test]
    fn verified_subject_memfd_passes_real_loader_gates() {
        let (backend, library) =
            verified_subject_library_from_env().expect("verify and seal real subject library");
        // SAFETY: the loader path names the hash-verified, write-sealed memfd held by `library`.
        let subject = unsafe { reth_dtvm_adapter::Dtvm::load_for(library.path(), backend) }
            .expect("load verified subject memfd");
        drop(subject);
        drop(library);
    }

    #[test]
    fn bad_receipts_commitment_fails_post_execution_validation() {
        let mut bundle = output_valid_empty_osaka_bundle();
        let mut block = target_block(&bundle);
        block.header.receipts_root = B256::repeat_byte(0x55);
        bind_target_block(&mut bundle, block);

        assert!(matches!(
            replay_bundle(bundle),
            Err(ReplayError::ReferencePostValidation(_))
        ));
    }

    #[test]
    fn create_transaction_reaches_sender_recovery() {
        let mut bundle = output_valid_empty_osaka_bundle();
        let header = target_block(&bundle).header;
        let transaction = invalid_signed_transaction(TxKind::Create);
        let mut block = Block::from_transactions(header, [transaction]);
        block.body.withdrawals = Some(Default::default());
        block.header.withdrawals_root = block.body.calculate_withdrawals_root();
        bind_target_block(&mut bundle, block);

        assert!(matches!(
            replay_bundle(bundle),
            Err(ReplayError::SenderRecovery)
        ));
    }

    #[test]
    fn type4_transaction_reaches_sender_recovery() {
        let mut bundle = output_valid_empty_osaka_bundle();
        let header = target_block(&bundle).header;
        let authorization = SignedAuthorization::new_unchecked(
            Authorization {
                chain_id: U256::from(1),
                address: Address::repeat_byte(0x55),
                nonce: 0,
            },
            0,
            U256::ZERO,
            U256::ZERO,
        );
        let transaction = Transaction::Eip7702(TxEip7702 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 2,
            max_priority_fee_per_gas: 0,
            to: Address::repeat_byte(0x44),
            value: U256::ZERO,
            access_list: AccessList::default(),
            authorization_list: vec![authorization],
            input: Bytes::new(),
        })
        .into_signed(Signature::new(U256::ZERO, U256::ZERO, false))
        .into();
        let mut block = Block::from_transactions(header, [transaction]);
        block.body.withdrawals = Some(Default::default());
        block.header.withdrawals_root = block.body.calculate_withdrawals_root();
        bind_target_block(&mut bundle, block);

        assert!(matches!(
            replay_bundle(bundle),
            Err(ReplayError::SenderRecovery)
        ));
    }

    fn output_valid_empty_osaka_bundle() -> WitnessBundle {
        let parent = Header {
            number: TARGET_NUMBER - 1,
            state_root: EMPTY_ROOT_HASH,
            gas_limit: 30_000_000,
            timestamp: TARGET_TIMESTAMP - 12,
            base_fee_per_gas: Some(1),
            withdrawals_root: Some(EMPTY_ROOT_HASH),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::repeat_byte(0x66)),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            ..Default::default()
        };
        let mut block = Block::from_transactions(
            Header {
                parent_hash: parent.hash_slow(),
                beneficiary: Address::repeat_byte(0x33),
                state_root: EMPTY_ROOT_HASH,
                receipts_root: EMPTY_ROOT_HASH,
                number: TARGET_NUMBER,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: TARGET_TIMESTAMP,
                mix_hash: B256::repeat_byte(0x77),
                base_fee_per_gas: Some(1),
                withdrawals_root: Some(EMPTY_ROOT_HASH),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::repeat_byte(0x88)),
                requests_hash: Some(EMPTY_REQUESTS_HASH),
                block_access_list_hash: None,
                slot_number: None,
                ..Default::default()
            },
            std::iter::empty(),
        );
        block.body.withdrawals = Some(Default::default());
        block.header.withdrawals_root = block.body.calculate_withdrawals_root();
        let raw = alloy_rlp::encode(&block);

        WitnessBundle {
            target_header: alloy_rlp::encode(&block.header).into(),
            target_block_hash: block.header.hash_slow(),
            target_block: Some(raw.into()),
            witness: ExecutionWitness {
                state: Vec::new(),
                codes: Vec::new(),
                keys: Vec::new(),
                headers: vec![Bytes::from(alloy_rlp::encode(parent))],
            },
            access_manifest: AccessManifest::default(),
        }
    }

    fn target_block(bundle: &WitnessBundle) -> Block {
        let mut input = bundle
            .target_block
            .as_ref()
            .expect("raw target block")
            .as_ref();
        let block = Block::decode_sealed(&mut input).expect("decode raw target block");
        assert!(input.is_empty());
        block.into_inner()
    }

    fn bind_target_block(bundle: &mut WitnessBundle, block: Block) {
        bundle.target_header = alloy_rlp::encode(&block.header).into();
        bundle.target_block_hash = block.header.hash_slow();
        bundle.target_block = Some(alloy_rlp::encode(block).into());
    }

    fn invalid_signed_transaction(kind: TxKind) -> TransactionSigned {
        let transaction = Transaction::Legacy(TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 2,
            gas_limit: 21_000,
            to: kind,
            value: U256::ZERO,
            input: Bytes::new(),
        });
        transaction
            .into_signed(Signature::new(U256::ZERO, U256::ZERO, false))
            .into()
    }

    fn temporary_library_source(contents: &[u8]) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "reth-dtvm-library-source-{}-{nonce}",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("write temporary library source");
        path
    }

    fn sha256(contents: &[u8]) -> String {
        Sha256::digest(contents)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn hot_metrics_observation_uses_checked_monotonic_deltas() {
        let before = AdapterDtvmEvmcHotMetrics {
            top_level_execute_count: 10,
            module_cache_hit_count: 20,
            module_cache_entry_count: 4,
            ..Default::default()
        };
        let after = AdapterDtvmEvmcHotMetrics {
            top_level_execute_count: 13,
            module_cache_hit_count: 25,
            module_cache_entry_count: 3,
            ..Default::default()
        };

        let observation = DtvmHotMetricsObservation::checked(before, after).unwrap();

        assert_eq!(observation.delta.top_level_execute_count, 3);
        assert_eq!(observation.delta.module_cache_hit_count, 5);
        assert_eq!(observation.delta.module_cache_entry_count_before, 4);
        assert_eq!(observation.delta.module_cache_entry_count_after, 3);
    }

    #[test]
    fn hot_metrics_observation_rejects_a_decreasing_counter() {
        let before = AdapterDtvmEvmcHotMetrics {
            module_cache_hit_count: 2,
            ..Default::default()
        };
        let after = AdapterDtvmEvmcHotMetrics {
            module_cache_hit_count: 1,
            ..Default::default()
        };

        assert!(matches!(
            DtvmHotMetricsObservation::checked(before, after),
            Err(ReplayError::HotMetricsNonMonotonic(
                "module_cache_hit_count"
            ))
        ));
    }

    #[derive(Debug)]
    struct OpaqueExecutionError(String);

    impl std::fmt::Display for OpaqueExecutionError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl Error for OpaqueExecutionError {}
}
