use alloy_consensus::Header;
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{Address, Bytes, B256};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_trie::EMPTY_ROOT_HASH;
use reth_dtvm_witness_db::{AccessManifest, WitnessBundle};
use reth_ethereum_primitives::Block;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

const TARGET_NUMBER: u64 = 24_000_000;
const TARGET_TIMESTAMP: u64 = 1_800_000_000;

#[test]
fn replay_block_defaults_to_differential() {
    let fixture = TempFixture::write(&output_valid_empty_osaka_bundle());
    let output = Command::new(env!("CARGO_BIN_EXE_replay-block"))
        .arg(&fixture.path)
        .output()
        .expect("run replay-block");
    assert_process_success(&output);

    let report = parse_report(&output);
    assert_eq!(report["replayMode"], "differential");
    assert_eq!(
        report["subjectBackend"],
        std::env::var("RETH_SUBJECT_BACKEND").expect("RETH_SUBJECT_BACKEND")
    );
    assert_eq!(report["differentialMatch"], true);
    assert_subject_diagnostics(&report);
    assert_strict_report(&report);
    assert_phase_is_ns(&report, "rethRevmExecute");
    assert_phase_is_ns(&report, "rethSubjectExecute");
}

#[test]
fn replay_block_reference_only_needs_no_subject() {
    let fixture = TempFixture::write(&output_valid_empty_osaka_bundle());
    let output = Command::new(env!("CARGO_BIN_EXE_replay-block"))
        .args(["--mode", "reference-only"])
        .arg(&fixture.path)
        .env_remove("RETH_SUBJECT_BACKEND")
        .env_remove("RETH_SUBJECT_LIBRARY")
        .env_remove("RETH_SUBJECT_LIBRARY_SHA256")
        .env_remove("DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION")
        .output()
        .expect("run reference-only replay-block");
    assert_process_success(&output);

    let report = parse_report(&output);
    assert_eq!(report["replayMode"], "reference-only");
    assert!(report["subjectBackend"].is_null());
    assert!(report["subjectVmCreateCount"].is_null());
    assert!(report["dtvmPhaseMetrics"].is_null());
    assert!(report["differentialMatch"].is_null());
    assert_strict_report(&report);
    assert_phase_is_ns(&report, "rethRevmExecute");
    assert!(report["phaseWallTimeNs"]["rethSubjectExecute"].is_null());
}

#[test]
fn replay_block_subject_only_skips_reference() {
    let fixture = TempFixture::write(&output_valid_empty_osaka_bundle());
    let output = Command::new(env!("CARGO_BIN_EXE_replay-block"))
        .args(["--mode", "subject-only"])
        .arg(&fixture.path)
        .output()
        .expect("run subject-only replay-block");
    assert_process_success(&output);

    let report = parse_report(&output);
    assert_eq!(report["replayMode"], "subject-only");
    assert_eq!(
        report["subjectBackend"],
        std::env::var("RETH_SUBJECT_BACKEND").expect("RETH_SUBJECT_BACKEND")
    );
    assert_subject_diagnostics(&report);
    assert!(report["differentialMatch"].is_null());
    assert_strict_report(&report);
    assert!(report["phaseWallTimeNs"]["rethRevmExecute"].is_null());
    assert_phase_is_ns(&report, "rethSubjectExecute");
}

#[test]
fn replay_block_rejects_unknown_mode() {
    let fixture = TempFixture::write(&output_valid_empty_osaka_bundle());
    let output = Command::new(env!("CARGO_BIN_EXE_replay-block"))
        .args(["--mode", "unknown"])
        .arg(&fixture.path)
        .output()
        .expect("run replay-block with unknown mode");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid replay mode \"unknown\""),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn parse_report(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("replay-block JSON output")
}

fn assert_strict_report(report: &serde_json::Value) {
    assert_eq!(report["rawBound"], true);
    assert_eq!(report["preExecutionCommitments"], true);
    assert_eq!(report["postExecutionCommitments"]["gasUsed"], true);
    assert_eq!(report["postExecutionCommitments"]["receiptsRoot"], true);
    assert_eq!(report["preStateRootVerified"], true);
    assert_eq!(report["postStateRootVerified"], true);
    assert_eq!(report["blockNumber"], TARGET_NUMBER);
    let phases = report["phaseWallTimeNs"]
        .as_object()
        .expect("phaseWallTimeNs object");
    assert_eq!(phases.len(), 4);
    assert_phase_is_ns(report, "prepareImportPreflight");
    assert_phase_is_ns(report, "validationRoots");
}

fn assert_subject_diagnostics(report: &serde_json::Value) {
    assert_eq!(report["subjectVmCreateCount"], 1);
    if report["subjectBackend"] == "dtvm-eager" {
        let phase_metrics = report["dtvmPhaseMetrics"]
            .as_object()
            .expect("dtvmPhaseMetrics object");
        match phase_metrics["statusCode"].as_i64() {
            Some(0) => {
                assert_eq!(phase_metrics["status"], "success");
                let metrics = phase_metrics["metrics"]
                    .as_object()
                    .expect("successful DTVM metrics object");
                assert_eq!(metrics.len(), 9);
                assert_eq!(metrics["version"], 1);
                assert_eq!(metrics["structSize"], 64);
                assert_eq!(metrics["profileGuidedJitTriggerCount"], 0);
                for name in [
                    "topLevelExecuteCount",
                    "topLevelExecuteWallNs",
                    "synchronousJitCompileAttemptCount",
                    "synchronousJitCompileSuccessCount",
                    "synchronousJitCompileWallNs",
                    "nonCompileResidualNs",
                ] {
                    assert!(metrics[name].as_u64().is_some(), "missing {name}");
                }
            }
            Some(2) => {
                assert_eq!(phase_metrics["status"], "disabled");
                assert!(phase_metrics["metrics"].is_null());
            }
            status => panic!("unexpected DTVM metrics status {status:?}: {phase_metrics:?}"),
        }
    } else {
        assert!(report["dtvmPhaseMetrics"].is_null());
    }
}

fn assert_phase_is_ns(report: &serde_json::Value, name: &str) {
    assert!(
        report["phaseWallTimeNs"][name].as_u64().is_some(),
        "{name} is a nonnegative ns value"
    );
}

fn assert_process_success(output: &Output) {
    assert!(
        output.status.success(),
        "replay-block failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

struct TempFixture {
    path: PathBuf,
}

impl TempFixture {
    fn write(bundle: &WitnessBundle) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "reth-dtvm-replay-block-{}-{nonce}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            serde_json::to_vec(bundle).expect("serialize replay fixture"),
        )
        .expect("write replay fixture");
        Self { path }
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
