//! Fixed-lifecycle replay for the instrumented evmone advanced C ABI.

use crate::{
    batch::{BatchCorrectness, BatchInput, BatchPass, FIXED_HOT_PASSES},
    replay::{
        EvmoneAdvancedMetricsDelta, EvmoneAdvancedMetricsObservation, ReplayError,
        ReplayEvmoneBatchSession, ReplayMode, ReplayReport, RunExecLoopMetrics,
    },
};
use serde::Serialize;
use std::io::{self, Write};
use thiserror::Error;

pub const EVMONE_BATCH_SCHEMA_VERSION: u32 = 1;
pub const EVMONE_BATCH_SCHEMA: &str = "dtvm.evmone-advanced-diagnostic-batch-block.v1";
pub const EVMONE_RESOURCE_BATCH_SCHEMA: &str = "dtvm.evmone-resource-batch-block.v1";
pub const EVMONE_INSTRUMENTATION_IDENTITY: &str = "evmone-advanced-diagnostic-v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmoneExecutionRows {
    pub reth_run_exec_loop: RunExecLoopMetrics,
    pub evmc_top_level_analysis_setup_and_core_count: u64,
    pub evmc_top_level_analysis_setup_and_core_wall_ns: u64,
    pub advanced_analysis_count: u64,
    pub advanced_analysis_wall_ns: u64,
    pub advanced_state_setup_count: u64,
    pub advanced_state_setup_wall_ns: u64,
    pub advanced_core_analysis_excluded_count: u64,
    pub advanced_core_analysis_excluded_wall_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmoneMetricsGate {
    pub passed: bool,
    pub violations: Vec<String>,
    pub phase_wall_sum_ns: Option<u64>,
    pub phase_wall_within_top_level: bool,
    pub top_level_wall_within_reth_run_exec_loop: bool,
}

impl EvmoneMetricsGate {
    fn evaluate(
        vm_create_count: Option<u64>,
        observation: Option<&EvmoneAdvancedMetricsObservation>,
        subject_loop: Option<RunExecLoopMetrics>,
    ) -> Self {
        let mut violations = Vec::new();
        if vm_create_count != Some(1) {
            violations.push(format!(
                "subjectVmCreateCount must equal 1, got {vm_create_count:?}"
            ));
        }
        let (Some(observation), Some(subject_loop)) = (observation, subject_loop) else {
            violations.push(
                "required evmone metrics or subject run_exec_loop delta is missing".to_string(),
            );
            return Self {
                passed: false,
                violations,
                phase_wall_sum_ns: None,
                phase_wall_within_top_level: false,
                top_level_wall_within_reth_run_exec_loop: false,
            };
        };
        let delta = &observation.delta;
        if delta.top_level_execute_count == 0 {
            violations.push("topLevelExecuteCount must be greater than zero".to_string());
        }
        for (field, value) in [
            ("advancedAnalysisCount", delta.advanced_analysis_count),
            ("advancedStateSetupCount", delta.advanced_state_setup_count),
            (
                "advancedCoreExecuteCount",
                delta.advanced_core_execute_count,
            ),
        ] {
            if value != delta.top_level_execute_count {
                violations.push(format!(
                    "{field} ({value}) must equal topLevelExecuteCount ({})",
                    delta.top_level_execute_count
                ));
            }
        }
        let phase_wall_sum_ns = [
            delta.advanced_analysis_wall_ns,
            delta.advanced_state_setup_wall_ns,
            delta.advanced_core_execute_wall_ns,
        ]
        .into_iter()
        .try_fold(0u64, u64::checked_add);
        let phase_wall_within_top_level =
            phase_wall_sum_ns.is_some_and(|wall| wall <= delta.top_level_execute_wall_ns);
        if !phase_wall_within_top_level {
            violations.push("evmone phase wall sum exceeds top-level EVMC wall".to_string());
        }
        let top_level_wall_within_reth_run_exec_loop =
            delta.top_level_execute_wall_ns <= subject_loop.wall_ns;
        if !top_level_wall_within_reth_run_exec_loop {
            violations.push("top-level EVMC wall exceeds Reth run_exec_loop wall".to_string());
        }
        Self {
            passed: violations.is_empty(),
            violations,
            phase_wall_sum_ns,
            phase_wall_within_top_level,
            top_level_wall_within_reth_run_exec_loop,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmoneBatchRecord<'a> {
    pub schema: &'static str,
    pub schema_version: u32,
    pub instrumentation_identity: &'static str,
    pub subject_library_sha256: &'a str,
    pub pass: BatchPass,
    pub block_index: usize,
    pub bundle_path: &'a str,
    pub correctness: BatchCorrectness,
    pub metrics_gate: EvmoneMetricsGate,
    pub execution_rows: Option<EvmoneExecutionRows>,
    pub metrics_delta: Option<EvmoneAdvancedMetricsDelta>,
    pub replay: ReplayReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmoneResourceCorrectness {
    pub passed: bool,
    pub raw_bound: bool,
    pub pre_execution_commitments: bool,
    pub post_execution_commitments: bool,
    pub pre_state_root_verified: bool,
    pub post_state_root_verified: bool,
    pub subject_only_boundary: bool,
}

impl EvmoneResourceCorrectness {
    fn from_report(report: &ReplayReport) -> Self {
        let post = &report.post_execution_commitments;
        let post_execution_commitments = post.gas_used
            && post.receipts_root
            && post.logs_bloom
            && post.requests_hash
            && post.blob_gas_used;
        let subject_only_boundary = report.replay_mode == ReplayMode::SubjectOnly
            && report
                .reth_subject_run_exec_loop
                .is_some_and(|metrics| metrics.call_count > 0)
            && report.reth_revm_run_exec_loop.is_none()
            && report.subject_vm_create_count == Some(1);
        let passed = report.raw_bound
            && report.pre_execution_commitments
            && post_execution_commitments
            && report.pre_state_root_verified
            && report.post_state_root_verified
            && subject_only_boundary;
        Self {
            passed,
            raw_bound: report.raw_bound,
            pre_execution_commitments: report.pre_execution_commitments,
            post_execution_commitments,
            pre_state_root_verified: report.pre_state_root_verified,
            post_state_root_verified: report.post_state_root_verified,
            subject_only_boundary,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmoneResourceRecord<'a> {
    pub schema: &'static str,
    pub resource_only: bool,
    pub instrumentation_identity: &'static str,
    pub subject_library_sha256: &'a str,
    pub pass: BatchPass,
    pub block_index: usize,
    pub bundle_path: &'a str,
    pub correctness: EvmoneResourceCorrectness,
    pub metrics_gate: EvmoneMetricsGate,
    pub execution_rows: Option<EvmoneExecutionRows>,
    pub metrics_delta: Option<EvmoneAdvancedMetricsDelta>,
    pub replay: ReplayReport,
}

#[derive(Debug, Error)]
pub enum EvmoneBatchError {
    #[error("at least one witness bundle is required")]
    NoInputs,
    #[error("--smoke-one and --resource-only are mutually exclusive")]
    ConflictingModes,
    #[error("RETH_SUBJECT_LIBRARY_SHA256 must be an exact lowercase SHA-256")]
    InvalidLibraryIdentity,
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error("failed to serialize JSONL record: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to write JSONL record: {0}")]
    Write(#[from] io::Error),
    #[error("correctness or metrics gate failed at {pass} block {block_index}: {reason}")]
    Gate {
        pass: &'static str,
        block_index: usize,
        reason: String,
    },
}

pub fn run_fixed_evmone_batch(
    inputs: &[BatchInput],
    output: impl Write,
) -> Result<(), EvmoneBatchError> {
    run_evmone_passes(inputs, &FIXED_HOT_PASSES, output)
}

/// One-record smoke path that exercises the real diagnostic ABI without entering measured passes.
pub fn run_evmone_smoke(inputs: &[BatchInput], output: impl Write) -> Result<(), EvmoneBatchError> {
    let first = inputs.first().ok_or(EvmoneBatchError::NoInputs)?;
    run_evmone_passes(std::slice::from_ref(first), &FIXED_HOT_PASSES[..1], output)
}

/// Engine-only fixed10 lifecycle for process-scoped perf/RSS collection.
/// It is not part of the timing sample set.
pub fn run_fixed_evmone_resource_batch(
    inputs: &[BatchInput],
    mut output: impl Write,
) -> Result<(), EvmoneBatchError> {
    if inputs.is_empty() {
        return Err(EvmoneBatchError::NoInputs);
    }
    let library_sha256 = subject_library_sha256()?;
    let session = ReplayEvmoneBatchSession::from_env()?;
    for pass in FIXED_HOT_PASSES {
        for (block_index, input) in inputs.iter().enumerate() {
            let replay = session.replay_json_with_mode(&input.json, ReplayMode::SubjectOnly)?;
            let correctness = EvmoneResourceCorrectness::from_report(&replay);
            let metrics_gate = EvmoneMetricsGate::evaluate(
                replay.subject_vm_create_count,
                replay.evmone_advanced_metrics.as_ref(),
                replay.reth_subject_run_exec_loop,
            );
            let execution_rows = replay
                .evmone_advanced_metrics
                .zip(replay.reth_subject_run_exec_loop)
                .map(|(metrics, reth_run_exec_loop)| EvmoneExecutionRows {
                    reth_run_exec_loop,
                    evmc_top_level_analysis_setup_and_core_count: metrics
                        .delta
                        .top_level_execute_count,
                    evmc_top_level_analysis_setup_and_core_wall_ns: metrics
                        .delta
                        .top_level_execute_wall_ns,
                    advanced_analysis_count: metrics.delta.advanced_analysis_count,
                    advanced_analysis_wall_ns: metrics.delta.advanced_analysis_wall_ns,
                    advanced_state_setup_count: metrics.delta.advanced_state_setup_count,
                    advanced_state_setup_wall_ns: metrics.delta.advanced_state_setup_wall_ns,
                    advanced_core_analysis_excluded_count: metrics
                        .delta
                        .advanced_core_execute_count,
                    advanced_core_analysis_excluded_wall_ns: metrics
                        .delta
                        .advanced_core_execute_wall_ns,
                });
            let bundle_path = input.path.to_string_lossy();
            let record = EvmoneResourceRecord {
                schema: EVMONE_RESOURCE_BATCH_SCHEMA,
                resource_only: true,
                instrumentation_identity: EVMONE_INSTRUMENTATION_IDENTITY,
                subject_library_sha256: &library_sha256,
                pass,
                block_index,
                bundle_path: &bundle_path,
                correctness: correctness.clone(),
                metrics_gate: metrics_gate.clone(),
                execution_rows,
                metrics_delta: replay.evmone_advanced_metrics.map(|metrics| metrics.delta),
                replay,
            };
            serde_json::to_writer(&mut output, &record)?;
            output.write_all(b"\n")?;
            output.flush()?;
            if !correctness.passed || !metrics_gate.passed {
                return Err(EvmoneBatchError::Gate {
                    pass: pass.label,
                    block_index,
                    reason: if !correctness.passed {
                        "single-engine correctness gate failed".to_string()
                    } else {
                        metrics_gate.violations.join("; ")
                    },
                });
            }
        }
    }
    Ok(())
}

fn run_evmone_passes(
    inputs: &[BatchInput],
    passes: &[BatchPass],
    mut output: impl Write,
) -> Result<(), EvmoneBatchError> {
    if inputs.is_empty() {
        return Err(EvmoneBatchError::NoInputs);
    }
    let library_sha256 = subject_library_sha256()?;
    let session = ReplayEvmoneBatchSession::from_env()?;
    for &pass in passes {
        for (block_index, input) in inputs.iter().enumerate() {
            let replay = session.replay_json(&input.json)?;
            let correctness = BatchCorrectness::from_report(&replay);
            let metrics_gate = EvmoneMetricsGate::evaluate(
                replay.subject_vm_create_count,
                replay.evmone_advanced_metrics.as_ref(),
                replay.reth_subject_run_exec_loop,
            );
            let execution_rows = replay
                .evmone_advanced_metrics
                .zip(replay.reth_subject_run_exec_loop)
                .map(|(metrics, reth_run_exec_loop)| EvmoneExecutionRows {
                    reth_run_exec_loop,
                    evmc_top_level_analysis_setup_and_core_count: metrics
                        .delta
                        .top_level_execute_count,
                    evmc_top_level_analysis_setup_and_core_wall_ns: metrics
                        .delta
                        .top_level_execute_wall_ns,
                    advanced_analysis_count: metrics.delta.advanced_analysis_count,
                    advanced_analysis_wall_ns: metrics.delta.advanced_analysis_wall_ns,
                    advanced_state_setup_count: metrics.delta.advanced_state_setup_count,
                    advanced_state_setup_wall_ns: metrics.delta.advanced_state_setup_wall_ns,
                    advanced_core_analysis_excluded_count: metrics
                        .delta
                        .advanced_core_execute_count,
                    advanced_core_analysis_excluded_wall_ns: metrics
                        .delta
                        .advanced_core_execute_wall_ns,
                });
            let bundle_path = input.path.to_string_lossy();
            let record = EvmoneBatchRecord {
                schema: EVMONE_BATCH_SCHEMA,
                schema_version: EVMONE_BATCH_SCHEMA_VERSION,
                instrumentation_identity: EVMONE_INSTRUMENTATION_IDENTITY,
                subject_library_sha256: &library_sha256,
                pass,
                block_index,
                bundle_path: &bundle_path,
                correctness: correctness.clone(),
                metrics_gate: metrics_gate.clone(),
                execution_rows,
                metrics_delta: replay.evmone_advanced_metrics.map(|metrics| metrics.delta),
                replay,
            };
            serde_json::to_writer(&mut output, &record)?;
            output.write_all(b"\n")?;
            output.flush()?;
            if !correctness.passed || !metrics_gate.passed || session.subject_vm_create_count() != 1
            {
                return Err(EvmoneBatchError::Gate {
                    pass: pass.label,
                    block_index,
                    reason: if !correctness.passed {
                        "correctness gate failed".to_string()
                    } else if !metrics_gate.passed {
                        metrics_gate.violations.join("; ")
                    } else {
                        format!(
                            "subject VM count changed to {}",
                            session.subject_vm_create_count()
                        )
                    },
                });
            }
        }
    }
    Ok(())
}

fn subject_library_sha256() -> Result<String, EvmoneBatchError> {
    let library_sha256 = std::env::var("RETH_SUBJECT_LIBRARY_SHA256")
        .map_err(|_| EvmoneBatchError::InvalidLibraryIdentity)?;
    if library_sha256.len() != 64
        || !library_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(EvmoneBatchError::InvalidLibraryIdentity);
    }
    Ok(library_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_accepts_consistent_analysis_setup_and_core_rows() {
        let metrics = EvmoneAdvancedMetricsObservation {
            before: Default::default(),
            after: Default::default(),
            delta: EvmoneAdvancedMetricsDelta {
                top_level_execute_count: 2,
                top_level_execute_wall_ns: 100,
                advanced_analysis_count: 2,
                advanced_analysis_wall_ns: 10,
                advanced_state_setup_count: 2,
                advanced_state_setup_wall_ns: 15,
                advanced_core_execute_count: 2,
                advanced_core_execute_wall_ns: 70,
            },
        };

        let gate = EvmoneMetricsGate::evaluate(
            Some(1),
            Some(&metrics),
            Some(RunExecLoopMetrics {
                call_count: 2,
                wall_ns: 120,
            }),
        );

        assert!(gate.passed, "{:?}", gate.violations);
        assert_eq!(gate.phase_wall_sum_ns, Some(95));
    }

    #[test]
    fn gate_fails_closed_without_diagnostic_metrics() {
        let gate = EvmoneMetricsGate::evaluate(
            Some(1),
            None,
            Some(RunExecLoopMetrics {
                call_count: 1,
                wall_ns: 1,
            }),
        );
        assert!(!gate.passed);
    }
}
