//! Fixed-lifecycle, single-threaded hot-cache replay worker.

use crate::replay::{
    DtvmHotMetricsDelta, DtvmHotMetricsObservation, ReplayBatchSession, ReplayError, ReplayMode,
    ReplayReferenceBatchSession, ReplayReport, RunExecLoopMetrics,
};
use serde::Serialize;
use std::{
    io::{self, Write},
    path::PathBuf,
};
use thiserror::Error;

pub const BATCH_SCHEMA_VERSION: u32 = 1;
pub const BATCH_SCHEMA: &str = "dtvm.reth-hot-cache-batch-block.v1";
pub const PRODUCTION_BATCH_SCHEMA: &str = "dtvm.reth-production-hot-batch-block.v1";
pub const PRODUCTION_RESOURCE_BATCH_SCHEMA: &str = "dtvm.reth-production-resource-batch-block.v1";
pub const RESOURCE_BATCH_SCHEMA: &str = "dtvm.reth-resource-batch-block.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceEngine {
    Dtvm,
    Revm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BatchStage {
    ColdPopulation,
    HotGate,
    Warmup,
    Measured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchPass {
    pub label: &'static str,
    pub pass_index: usize,
    pub stage: BatchStage,
    pub stage_index: usize,
    pub hot_cache_required: bool,
    pub measured: bool,
}

pub const FIXED_HOT_PASSES: [BatchPass; 12] = [
    BatchPass {
        label: "C0",
        pass_index: 0,
        stage: BatchStage::ColdPopulation,
        stage_index: 0,
        hot_cache_required: false,
        measured: false,
    },
    BatchPass {
        label: "G0",
        pass_index: 1,
        stage: BatchStage::HotGate,
        stage_index: 0,
        hot_cache_required: true,
        measured: false,
    },
    BatchPass {
        label: "W0",
        pass_index: 2,
        stage: BatchStage::Warmup,
        stage_index: 0,
        hot_cache_required: true,
        measured: false,
    },
    BatchPass {
        label: "W1",
        pass_index: 3,
        stage: BatchStage::Warmup,
        stage_index: 1,
        hot_cache_required: true,
        measured: false,
    },
    BatchPass {
        label: "M0",
        pass_index: 4,
        stage: BatchStage::Measured,
        stage_index: 0,
        hot_cache_required: true,
        measured: true,
    },
    BatchPass {
        label: "M1",
        pass_index: 5,
        stage: BatchStage::Measured,
        stage_index: 1,
        hot_cache_required: true,
        measured: true,
    },
    BatchPass {
        label: "M2",
        pass_index: 6,
        stage: BatchStage::Measured,
        stage_index: 2,
        hot_cache_required: true,
        measured: true,
    },
    BatchPass {
        label: "M3",
        pass_index: 7,
        stage: BatchStage::Measured,
        stage_index: 3,
        hot_cache_required: true,
        measured: true,
    },
    BatchPass {
        label: "M4",
        pass_index: 8,
        stage: BatchStage::Measured,
        stage_index: 4,
        hot_cache_required: true,
        measured: true,
    },
    BatchPass {
        label: "M5",
        pass_index: 9,
        stage: BatchStage::Measured,
        stage_index: 5,
        hot_cache_required: true,
        measured: true,
    },
    BatchPass {
        label: "M6",
        pass_index: 10,
        stage: BatchStage::Measured,
        stage_index: 6,
        hot_cache_required: true,
        measured: true,
    },
    BatchPass {
        label: "M7",
        pass_index: 11,
        stage: BatchStage::Measured,
        stage_index: 7,
        hot_cache_required: true,
        measured: true,
    },
];

#[derive(Debug)]
pub struct BatchInput {
    pub path: PathBuf,
    pub json: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchCorrectness {
    pub passed: bool,
    pub differential_match: bool,
    pub raw_bound: bool,
    pub pre_execution_commitments: bool,
    pub post_execution_commitments: bool,
    pub pre_state_root_verified: bool,
    pub post_state_root_verified: bool,
    pub common_run_exec_loop_boundary: bool,
    pub run_exec_loop_call_count_match: bool,
}

impl BatchCorrectness {
    pub(crate) fn from_report(report: &ReplayReport) -> Self {
        let post = &report.post_execution_commitments;
        let post_execution_commitments = post.gas_used
            && post.receipts_root
            && post.logs_bloom
            && post.requests_hash
            && post.blob_gas_used;
        let common_run_exec_loop_boundary = report
            .reth_revm_run_exec_loop
            .zip(report.reth_subject_run_exec_loop)
            .is_some_and(|(reference, subject)| reference.call_count > 0 && subject.call_count > 0);
        let run_exec_loop_call_count_match = report
            .reth_revm_run_exec_loop
            .zip(report.reth_subject_run_exec_loop)
            .is_some_and(|(reference, subject)| reference.call_count == subject.call_count);
        let differential_match = report.differential_match == Some(true);
        let passed = differential_match
            && report.raw_bound
            && report.pre_execution_commitments
            && post_execution_commitments
            && report.pre_state_root_verified
            && report.post_state_root_verified
            && common_run_exec_loop_boundary
            && run_exec_loop_call_count_match;
        Self {
            passed,
            differential_match,
            raw_bound: report.raw_bound,
            pre_execution_commitments: report.pre_execution_commitments,
            post_execution_commitments,
            pre_state_root_verified: report.pre_state_root_verified,
            post_state_root_verified: report.post_state_root_verified,
            common_run_exec_loop_boundary,
            run_exec_loop_call_count_match,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotCacheGate {
    pub required: bool,
    pub passed: bool,
    pub violations: Vec<String>,
    pub frame_coverage: Option<FrameCoverage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameCoverage {
    pub top_level_execute_count: u64,
    pub jit_frame_count: u64,
    pub interpreter_frame_count: u64,
    pub total_frame_count: Option<u64>,
    pub frames_cover_top_level_executes: bool,
    pub classified_interpreter_fallback_count: Option<u64>,
    pub unclassified_interpreter_frame_count: Option<u64>,
    pub fallback_classification_within_interpreter_frames: bool,
    pub jit_active_wall_ns: u64,
    pub interpreter_active_wall_ns: u64,
    pub total_active_wall_ns: Option<u64>,
    pub top_level_execute_wall_ns: u64,
    pub active_wall_within_top_level_execute_wall: bool,
}

impl HotCacheGate {
    fn evaluate(
        vm_create_count: Option<u64>,
        observation: Option<&DtvmHotMetricsObservation>,
        required: bool,
    ) -> Self {
        let mut violations = Vec::new();
        if vm_create_count != Some(1) {
            violations.push(format!(
                "subjectVmCreateCount must equal 1, got {vm_create_count:?}"
            ));
        }
        let Some(observation) = observation else {
            violations.push("required DTVM metrics v2 observation is missing".to_string());
            return Self {
                required,
                passed: false,
                violations,
                frame_coverage: None,
            };
        };
        let delta = &observation.delta;
        let total_frame_count =
            checked_sum(&[delta.jit_frame_count, delta.interpreter_frame_count]);
        let classified_interpreter_fallback_count = checked_sum(&[
            delta.create_interpreter_fallback_count,
            delta.newly_created_interpreter_fallback_count,
            delta.small_code_interpreter_fallback_count,
            delta.sticky_interpreter_fallback_count,
        ]);
        let unclassified_interpreter_frame_count = classified_interpreter_fallback_count
            .and_then(|classified| delta.interpreter_frame_count.checked_sub(classified));
        let total_active_wall_ns =
            checked_sum(&[delta.jit_active_wall_ns, delta.interpreter_active_wall_ns]);
        let active_wall_within_top_level_execute_wall =
            total_active_wall_ns.is_some_and(|active| active <= delta.top_level_execute_wall_ns);
        let frame_coverage = FrameCoverage {
            top_level_execute_count: delta.top_level_execute_count,
            jit_frame_count: delta.jit_frame_count,
            interpreter_frame_count: delta.interpreter_frame_count,
            total_frame_count,
            frames_cover_top_level_executes: total_frame_count
                .is_some_and(|frames| frames >= delta.top_level_execute_count),
            classified_interpreter_fallback_count,
            unclassified_interpreter_frame_count,
            fallback_classification_within_interpreter_frames: unclassified_interpreter_frame_count
                .is_some(),
            jit_active_wall_ns: delta.jit_active_wall_ns,
            interpreter_active_wall_ns: delta.interpreter_active_wall_ns,
            total_active_wall_ns,
            top_level_execute_wall_ns: delta.top_level_execute_wall_ns,
            active_wall_within_top_level_execute_wall,
        };
        if required {
            require_zero(
                &mut violations,
                "synchronousJitCompileAttemptCount",
                delta.synchronous_jit_compile_attempt_count,
            );
            require_zero(
                &mut violations,
                "synchronousJitCompileSuccessCount",
                delta.synchronous_jit_compile_success_count,
            );
            require_zero(
                &mut violations,
                "synchronousJitCompileWallNs",
                delta.synchronous_jit_compile_wall_ns,
            );
            require_zero(
                &mut violations,
                "profileGuidedJitTriggerCount",
                delta.profile_guided_jit_trigger_count,
            );
            require_zero(
                &mut violations,
                "moduleCacheMissCount",
                delta.module_cache_miss_count,
            );
            require_zero(
                &mut violations,
                "moduleCacheValidationRejectCount",
                delta.module_cache_validation_reject_count,
            );
            require_zero(
                &mut violations,
                "moduleCacheEvictionCount",
                delta.module_cache_eviction_count,
            );
            if delta.top_level_execute_count == 0 {
                violations.push("topLevelExecuteCount must be greater than zero".to_string());
            }
            match total_active_wall_ns {
                Some(_) if active_wall_within_top_level_execute_wall => {}
                Some(active_wall_ns) => violations.push(format!(
                    "jitActiveWallNs + interpreterActiveWallNs ({active_wall_ns}) exceeds \
                     topLevelExecuteWallNs ({})",
                    delta.top_level_execute_wall_ns
                )),
                None => violations.push("active frame wall times overflowed u64".to_string()),
            }
        }
        Self {
            required,
            passed: violations.is_empty(),
            violations,
            frame_coverage: Some(frame_coverage),
        }
    }
}

fn require_zero(violations: &mut Vec<String>, field: &str, value: u64) {
    if value != 0 {
        violations.push(format!("{field} must equal 0, got {value}"));
    }
}

fn checked_sum(values: &[u64]) -> Option<u64> {
    values
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchBlockRecord<'a> {
    pub schema: &'static str,
    pub schema_version: u32,
    pub timing_role: &'static str,
    pub library_role: &'static str,
    pub metrics_enabled: bool,
    pub timing_use: bool,
    pub pass: BatchPass,
    pub block_index: usize,
    pub bundle_path: &'a str,
    pub correctness: BatchCorrectness,
    pub cache_counter_delta: Option<DtvmHotMetricsDelta>,
    pub hot_cache_gate: HotCacheGate,
    pub replay: ReplayReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductionLifecycleProof {
    pub passed: bool,
    pub subject_vm_create_count: Option<u64>,
    pub single_subject_vm: bool,
    pub fresh_witness_db_executor_journal_host_per_block: bool,
    pub phase_metrics_disabled: bool,
    pub diagnostic_metrics_absent: bool,
    pub run_exec_loop_boundary_observed: bool,
    pub metrics_gate_applied: bool,
}

impl ProductionLifecycleProof {
    fn from_report(session: &ReplayBatchSession, report: &ReplayReport) -> Self {
        let single_subject_vm =
            session.subject_vm_create_count() == 1 && report.subject_vm_create_count == Some(1);
        let phase_metrics_disabled = session.phase_metrics_disabled();
        let diagnostic_metrics_absent = report.dtvm_phase_metrics.as_ref().is_some_and(|metrics| {
            metrics.status.as_str() == "disabled" && metrics.metrics.is_none()
        }) && report.dtvm_hot_metrics.is_none()
            && report.evmone_advanced_metrics.is_none();
        let run_exec_loop_boundary_observed = report
            .reth_subject_run_exec_loop
            .is_some_and(|metrics| metrics.call_count > 0);
        Self {
            passed: single_subject_vm
                && phase_metrics_disabled
                && diagnostic_metrics_absent
                && run_exec_loop_boundary_observed,
            subject_vm_create_count: report.subject_vm_create_count,
            single_subject_vm,
            // replay_bundle_started constructs these for every call; only the
            // EVMC subject in ReplayBatchSession is shared.
            fresh_witness_db_executor_journal_host_per_block: true,
            phase_metrics_disabled,
            diagnostic_metrics_absent,
            run_exec_loop_boundary_observed,
            metrics_gate_applied: false,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductionBatchBlockRecord<'a> {
    pub schema: &'static str,
    pub schema_version: u32,
    pub timing_role: &'static str,
    pub library_role: &'static str,
    pub metrics_enabled: bool,
    pub timing_use: bool,
    pub requires_same_cell_diagnostic_qualification: bool,
    pub pass: BatchPass,
    pub block_index: usize,
    pub bundle_path: &'a str,
    pub correctness: BatchCorrectness,
    pub lifecycle: ProductionLifecycleProof,
    pub replay: ReplayReport,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductionResourceBatchRecord<'a> {
    pub schema: &'static str,
    pub resource_only: bool,
    pub library_role: &'static str,
    pub metrics_enabled: bool,
    pub timing_use: bool,
    pub pass: BatchPass,
    pub block_index: usize,
    pub bundle_path: &'a str,
    pub correctness: ResourceCorrectness,
    pub lifecycle: ProductionLifecycleProof,
    pub run_exec_loop: Option<RunExecLoopMetrics>,
    pub replay: ReplayReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceCorrectness {
    pub passed: bool,
    pub raw_bound: bool,
    pub pre_execution_commitments: bool,
    pub post_execution_commitments: bool,
    pub pre_state_root_verified: bool,
    pub post_state_root_verified: bool,
    pub expected_replay_mode: bool,
    pub expected_engine_boundary: bool,
}

impl ResourceCorrectness {
    fn from_report(report: &ReplayReport, engine: ResourceEngine) -> Self {
        let post = &report.post_execution_commitments;
        let post_execution_commitments = post.gas_used
            && post.receipts_root
            && post.logs_bloom
            && post.requests_hash
            && post.blob_gas_used;
        let (expected_replay_mode, expected_engine_boundary) = match engine {
            ResourceEngine::Dtvm => (
                report.replay_mode == ReplayMode::SubjectOnly,
                report
                    .reth_subject_run_exec_loop
                    .is_some_and(|metrics| metrics.call_count > 0)
                    && report.reth_revm_run_exec_loop.is_none()
                    && report.subject_vm_create_count == Some(1),
            ),
            ResourceEngine::Revm => (
                report.replay_mode == ReplayMode::ReferenceOnly,
                report
                    .reth_revm_run_exec_loop
                    .is_some_and(|metrics| metrics.call_count > 0)
                    && report.reth_subject_run_exec_loop.is_none()
                    && report.subject_vm_create_count.is_none(),
            ),
        };
        let passed = report.raw_bound
            && report.pre_execution_commitments
            && post_execution_commitments
            && report.pre_state_root_verified
            && report.post_state_root_verified
            && expected_replay_mode
            && expected_engine_boundary;
        Self {
            passed,
            raw_bound: report.raw_bound,
            pre_execution_commitments: report.pre_execution_commitments,
            post_execution_commitments,
            pre_state_root_verified: report.pre_state_root_verified,
            post_state_root_verified: report.post_state_root_verified,
            expected_replay_mode,
            expected_engine_boundary,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceBatchRecord<'a> {
    pub schema: &'static str,
    pub resource_only: bool,
    pub engine: ResourceEngine,
    pub pass: BatchPass,
    pub block_index: usize,
    pub bundle_path: &'a str,
    pub correctness: ResourceCorrectness,
    pub run_exec_loop: Option<RunExecLoopMetrics>,
    pub cache_counter_delta: Option<DtvmHotMetricsDelta>,
    pub hot_cache_gate: Option<HotCacheGate>,
    pub replay: ReplayReport,
}

#[derive(Debug, Error)]
pub enum BatchError {
    #[error("at least one witness bundle is required")]
    NoInputs,
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error("failed to serialize JSONL record: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to write JSONL record: {0}")]
    Write(#[from] io::Error),
    #[error("correctness gate failed at {pass} block {block_index}")]
    Correctness {
        pass: &'static str,
        block_index: usize,
    },
    #[error("hot-cache gate failed at {pass} block {block_index}: {violations}")]
    HotCache {
        pass: &'static str,
        block_index: usize,
        violations: String,
    },
    #[error("production lifecycle gate failed at {pass} block {block_index}")]
    ProductionLifecycle {
        pass: &'static str,
        block_index: usize,
    },
}

/// Runs C0/G0, two warmups, and eight measured passes in one long-lived worker.
pub fn run_fixed_hot_batch(inputs: &[BatchInput], output: impl Write) -> Result<(), BatchError> {
    run_hot_passes(inputs, &FIXED_HOT_PASSES, output)
}

/// Runs the fixed lifecycle with a metrics-OFF production library. Cache and
/// frame qualification belongs to the separately required diagnostic run.
pub fn run_fixed_production_batch(
    inputs: &[BatchInput],
    mut output: impl Write,
) -> Result<(), BatchError> {
    if inputs.is_empty() {
        return Err(BatchError::NoInputs);
    }
    let session = ReplayBatchSession::from_env_production()?;
    // Measurement affordance, off by default. The protocol is pass-major: every block
    // runs under C0, then every block under G0, and so on, so a block's modules are
    // touched again only after the whole corpus has gone by. That is what a syncing
    // node does -- a block arrives once -- and it is why an undersized module cache
    // shows up as eviction here. Setting RETH_BATCH_BLOCK_MAJOR=1 inverts the loops so
    // each block runs its twelve passes back to back, which keeps that block's modules
    // resident throughout. Useful for asking how much of a result is ordering; NOT the
    // protocol, because it hides exactly the cache pressure a node would hit.
    let block_major = std::env::var_os("RETH_BATCH_BLOCK_MAJOR").is_some();
    let order: Vec<(BatchPass, usize)> = if block_major {
        (0..inputs.len())
            .flat_map(|b| FIXED_HOT_PASSES.into_iter().map(move |p| (p, b)))
            .collect()
    } else {
        FIXED_HOT_PASSES
            .into_iter()
            .flat_map(|p| (0..inputs.len()).map(move |b| (p, b)))
            .collect()
    };
    {
        {
            for (pass, block_index) in order {
                let input = &inputs[block_index];
            let replay = session.replay_json(&input.json)?;
            let correctness = BatchCorrectness::from_report(&replay);
            let lifecycle = ProductionLifecycleProof::from_report(&session, &replay);
            let bundle_path = input.path.to_string_lossy();
            let record = ProductionBatchBlockRecord {
                schema: PRODUCTION_BATCH_SCHEMA,
                schema_version: BATCH_SCHEMA_VERSION,
                timing_role: "production",
                library_role: "production",
                metrics_enabled: false,
                timing_use: pass.measured,
                requires_same_cell_diagnostic_qualification: true,
                pass,
                block_index,
                bundle_path: &bundle_path,
                correctness: correctness.clone(),
                lifecycle: lifecycle.clone(),
                replay,
            };
            serde_json::to_writer(&mut output, &record)?;
            output.write_all(b"\n")?;
            output.flush()?;
            if !correctness.passed {
                return Err(BatchError::Correctness {
                    pass: pass.label,
                    block_index,
                });
            }
            if !lifecycle.passed {
                return Err(BatchError::ProductionLifecycle {
                    pass: pass.label,
                    block_index,
                });
            }
        }
    }
    Ok(())
}

/// Production-library, subject-only pass for process-scoped perf and RSS.
/// Cache occupancy remains a separate metrics-ON diagnostic artifact.
pub fn run_fixed_production_resource_batch(
    inputs: &[BatchInput],
    mut output: impl Write,
) -> Result<(), BatchError> {
    if inputs.is_empty() {
        return Err(BatchError::NoInputs);
    }
    let session = ReplayBatchSession::from_env_production()?;
    for pass in FIXED_HOT_PASSES {
        for (block_index, input) in inputs.iter().enumerate() {
            let replay = session.replay_json_with_mode(&input.json, ReplayMode::SubjectOnly)?;
            let correctness = ResourceCorrectness::from_report(&replay, ResourceEngine::Dtvm);
            let lifecycle = ProductionLifecycleProof::from_report(&session, &replay);
            let bundle_path = input.path.to_string_lossy();
            let record = ProductionResourceBatchRecord {
                schema: PRODUCTION_RESOURCE_BATCH_SCHEMA,
                resource_only: true,
                library_role: "production",
                metrics_enabled: false,
                timing_use: false,
                pass,
                block_index,
                bundle_path: &bundle_path,
                correctness: correctness.clone(),
                lifecycle: lifecycle.clone(),
                run_exec_loop: replay.reth_subject_run_exec_loop,
                replay,
            };
            serde_json::to_writer(&mut output, &record)?;
            output.write_all(b"\n")?;
            output.flush()?;
            if !correctness.passed {
                return Err(BatchError::Correctness {
                    pass: pass.label,
                    block_index,
                });
            }
            if !lifecycle.passed {
                return Err(BatchError::ProductionLifecycle {
                    pass: pass.label,
                    block_index,
                });
            }
        }
    }
    Ok(())
}

/// Full-corpus qualification: populate on C0 and require zero hot-cache misses
/// and compilation on G0 before any warmup or measured pass is permitted.
pub fn run_cold_hot_gate_pilot(
    inputs: &[BatchInput],
    output: impl Write,
) -> Result<(), BatchError> {
    run_hot_passes(inputs, &FIXED_HOT_PASSES[..2], output)
}

fn run_hot_passes(
    inputs: &[BatchInput],
    passes: &[BatchPass],
    mut output: impl Write,
) -> Result<(), BatchError> {
    if inputs.is_empty() {
        return Err(BatchError::NoInputs);
    }
    let session = ReplayBatchSession::from_env()?;
    for &pass in passes {
        for (block_index, input) in inputs.iter().enumerate() {
            let replay = session.replay_json(&input.json)?;
            let correctness = BatchCorrectness::from_report(&replay);
            let hot_cache_gate = HotCacheGate::evaluate(
                replay.subject_vm_create_count,
                replay.dtvm_hot_metrics.as_ref(),
                pass.hot_cache_required,
            );
            let bundle_path = input.path.to_string_lossy();
            let record = BatchBlockRecord {
                schema: BATCH_SCHEMA,
                schema_version: BATCH_SCHEMA_VERSION,
                timing_role: "diagnostic",
                library_role: "metrics",
                metrics_enabled: true,
                timing_use: false,
                pass,
                block_index,
                bundle_path: &bundle_path,
                correctness: correctness.clone(),
                cache_counter_delta: replay.dtvm_hot_metrics.map(|metrics| metrics.delta),
                hot_cache_gate: hot_cache_gate.clone(),
                replay,
            };
            serde_json::to_writer(&mut output, &record)?;
            output.write_all(b"\n")?;
            output.flush()?;
            if !correctness.passed {
                return Err(BatchError::Correctness {
                    pass: pass.label,
                    block_index,
                });
            }
            if !hot_cache_gate.passed {
                return Err(BatchError::HotCache {
                    pass: pass.label,
                    block_index,
                    violations: hot_cache_gate.violations.join("; "),
                });
            }
            if session.subject_vm_create_count() != 1 {
                return Err(BatchError::HotCache {
                    pass: pass.label,
                    block_index,
                    violations: format!(
                        "session subjectVmCreateCount changed to {}",
                        session.subject_vm_create_count()
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Runs the same long-lived fixed10 lifecycle with one engine only. This is
/// intentionally for process-scoped perf/RSS characterization, not timing.
pub fn run_fixed_resource_batch(
    inputs: &[BatchInput],
    engine: ResourceEngine,
    mut output: impl Write,
) -> Result<(), BatchError> {
    if inputs.is_empty() {
        return Err(BatchError::NoInputs);
    }
    match engine {
        ResourceEngine::Dtvm => {
            let session = ReplayBatchSession::from_env()?;
            for pass in FIXED_HOT_PASSES {
                for (block_index, input) in inputs.iter().enumerate() {
                    let replay =
                        session.replay_json_with_mode(&input.json, ReplayMode::SubjectOnly)?;
                    let correctness = ResourceCorrectness::from_report(&replay, engine);
                    let gate = HotCacheGate::evaluate(
                        replay.subject_vm_create_count,
                        replay.dtvm_hot_metrics.as_ref(),
                        pass.hot_cache_required,
                    );
                    write_resource_record(
                        &mut output,
                        engine,
                        pass,
                        block_index,
                        input,
                        correctness.clone(),
                        replay.reth_subject_run_exec_loop,
                        replay.dtvm_hot_metrics.map(|metrics| metrics.delta),
                        Some(gate.clone()),
                        replay,
                    )?;
                    if !correctness.passed {
                        return Err(BatchError::Correctness {
                            pass: pass.label,
                            block_index,
                        });
                    }
                    if !gate.passed {
                        return Err(BatchError::HotCache {
                            pass: pass.label,
                            block_index,
                            violations: gate.violations.join("; "),
                        });
                    }
                }
            }
        }
        ResourceEngine::Revm => {
            let session = ReplayReferenceBatchSession::new()?;
            for pass in FIXED_HOT_PASSES {
                for (block_index, input) in inputs.iter().enumerate() {
                    let replay = session.replay_json(&input.json)?;
                    let correctness = ResourceCorrectness::from_report(&replay, engine);
                    write_resource_record(
                        &mut output,
                        engine,
                        pass,
                        block_index,
                        input,
                        correctness.clone(),
                        replay.reth_revm_run_exec_loop,
                        None,
                        None,
                        replay,
                    )?;
                    if !correctness.passed {
                        return Err(BatchError::Correctness {
                            pass: pass.label,
                            block_index,
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_resource_record(
    output: &mut impl Write,
    engine: ResourceEngine,
    pass: BatchPass,
    block_index: usize,
    input: &BatchInput,
    correctness: ResourceCorrectness,
    run_exec_loop: Option<RunExecLoopMetrics>,
    cache_counter_delta: Option<DtvmHotMetricsDelta>,
    hot_cache_gate: Option<HotCacheGate>,
    replay: ReplayReport,
) -> Result<(), BatchError> {
    let bundle_path = input.path.to_string_lossy();
    let record = ResourceBatchRecord {
        schema: RESOURCE_BATCH_SCHEMA,
        resource_only: true,
        engine,
        pass,
        block_index,
        bundle_path: &bundle_path,
        correctness,
        run_exec_loop,
        cache_counter_delta,
        hot_cache_gate,
        replay,
    };
    serde_json::to_writer(&mut *output, &record)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_plan_is_cold_gate_two_warmups_and_eight_measurements() {
        assert_eq!(FIXED_HOT_PASSES.len(), 12);
        assert_eq!(
            FIXED_HOT_PASSES.map(|pass| pass.label),
            ["C0", "G0", "W0", "W1", "M0", "M1", "M2", "M3", "M4", "M5", "M6", "M7"]
        );
        assert!(!FIXED_HOT_PASSES[0].hot_cache_required);
        assert!(FIXED_HOT_PASSES[1..]
            .iter()
            .all(|pass| pass.hot_cache_required));
        assert_eq!(
            FIXED_HOT_PASSES
                .iter()
                .filter(|pass| pass.stage == BatchStage::Warmup)
                .count(),
            2
        );
        assert_eq!(
            FIXED_HOT_PASSES.iter().filter(|pass| pass.measured).count(),
            8
        );
    }

    #[test]
    fn gate_fails_closed_without_metrics() {
        let gate = HotCacheGate::evaluate(Some(1), None, true);
        assert!(!gate.passed);
        assert_eq!(
            gate.violations,
            ["required DTVM metrics v2 observation is missing"]
        );
    }

    #[test]
    fn production_schema_cannot_be_mistaken_for_diagnostic_timing() {
        assert_ne!(PRODUCTION_BATCH_SCHEMA, BATCH_SCHEMA);
        assert_eq!(
            PRODUCTION_BATCH_SCHEMA,
            "dtvm.reth-production-hot-batch-block.v1"
        );
        assert_ne!(PRODUCTION_RESOURCE_BATCH_SCHEMA, RESOURCE_BATCH_SCHEMA);
        assert_eq!(
            PRODUCTION_RESOURCE_BATCH_SCHEMA,
            "dtvm.reth-production-resource-batch-block.v1"
        );
    }

    #[test]
    fn hot_gate_allows_a_classified_interpreter_fallback() {
        let delta = DtvmHotMetricsDelta {
            top_level_execute_count: 2,
            top_level_execute_wall_ns: 100,
            jit_frame_count: 2,
            jit_active_wall_ns: 60,
            interpreter_frame_count: 1,
            interpreter_active_wall_ns: 30,
            small_code_interpreter_fallback_count: 1,
            ..DtvmHotMetricsDelta::default()
        };
        let observation = DtvmHotMetricsObservation {
            before: Default::default(),
            after: Default::default(),
            delta,
        };

        let gate = HotCacheGate::evaluate(Some(1), Some(&observation), true);

        assert!(gate.passed, "{:?}", gate.violations);
        let coverage = gate.frame_coverage.unwrap();
        assert_eq!(coverage.total_frame_count, Some(3));
        assert!(coverage.frames_cover_top_level_executes);
        assert_eq!(coverage.unclassified_interpreter_frame_count, Some(0));
    }

    #[test]
    fn hot_gate_rejects_compile_but_records_unclassified_interpreter_frames() {
        let delta = DtvmHotMetricsDelta {
            top_level_execute_count: 1,
            top_level_execute_wall_ns: 100,
            synchronous_jit_compile_attempt_count: 1,
            interpreter_frame_count: 1,
            interpreter_active_wall_ns: 30,
            ..DtvmHotMetricsDelta::default()
        };
        let observation = DtvmHotMetricsObservation {
            before: Default::default(),
            after: Default::default(),
            delta,
        };

        let gate = HotCacheGate::evaluate(Some(1), Some(&observation), true);

        assert!(!gate.passed);
        assert!(gate
            .violations
            .iter()
            .any(|violation| violation.contains("synchronousJitCompileAttemptCount")));
        let coverage = gate.frame_coverage.unwrap();
        assert_eq!(coverage.unclassified_interpreter_frame_count, Some(1));
        assert!(coverage.fallback_classification_within_interpreter_frames);
    }
}
