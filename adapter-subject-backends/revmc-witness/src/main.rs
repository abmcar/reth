mod strict_db;
mod witness;

use alloy_evm::{
    block::{BlockExecutionResult, BlockExecutorFactory, TxResult as _},
    eth::EthEvmFactory,
    EvmFactory,
};
use alloy_primitives::{keccak256, Address, Bytes, B256};
use reth_chainspec::MAINNET;
use reth_consensus::{Consensus, HeaderValidator};
use reth_ethereum_consensus::{validate_block_post_execution, EthBeaconConsensus};
use reth_ethereum_primitives::{Block, EthPrimitives, Receipt};
use reth_evm::{execute::BlockExecutor, ConfigureEvm};
use reth_evm_ethereum::{
    factory::{JitBackend, JitMode, RethEvmFactory, RuntimeConfig, RuntimeStatsSnapshot},
    revm_spec, EthEvmConfig,
};
use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};
use revm::{
    context_interface::result::{ExecutionResult, HaltReason},
    database::{states::bundle_state::BundleRetention, BundleState, State},
    primitives::hardfork::SpecId,
};
use revmc::runtime::{LookupRequest, RuntimeCacheKey, RuntimeTuning};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fs,
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use witness::{WitnessBundle, WitnessDb};

const RETH_COMMIT: &str = "70fb52e5fc7e6fb799937005ac294c8fffba5a61";
const REVMC_COMMIT: &str = "cf68a87f627299a9c49bcc333a8a317c9b312a3d";
const JIT_WORKERS: usize = 16;
const COMPILE_GATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

type TxResult = ExecutionResult<HaltReason>;
type AnyResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Lane {
    /// Resident correctness proof only. It deliberately emits no elapsed time.
    CorrectnessSmoke,
    /// Starts before backend construction/compile dispatch and ends after block execution.
    FullLifecycle,
    /// Starts only after every requested program is resident and the runtime is quiescent.
    Resident,
}

impl Lane {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "correctness-smoke" => Some(Self::CorrectnessSmoke),
            "full-lifecycle" => Some(Self::FullLifecycle),
            "resident" => Some(Self::Resident),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Args {
    lane: Lane,
    bundles: Vec<PathBuf>,
}

#[derive(Debug)]
struct PreparedBlock {
    path: PathBuf,
    input_sha256: String,
    raw_bytes: usize,
    block_number: u64,
    block_hash: B256,
    expected_state_root: B256,
    pre_state_root: B256,
    recovered: RecoveredBlock<Block>,
    reference_db: Option<WitnessDb>,
    subject_db: Option<WitnessDb>,
}

#[derive(Debug)]
struct ExecutedBlock {
    result: BlockExecutionResult<Receipt>,
    tx_results: Vec<TxResult>,
    accesses: Vec<strict_db::DbAccess>,
    bundle_state: BundleState,
    db: WitnessDb,
}

#[derive(Debug)]
struct VerifiedReference {
    result: BlockExecutionResult<Receipt>,
    tx_results: Vec<TxResult>,
    accesses: Vec<strict_db::DbAccess>,
    bundle_state: BundleState,
    post_state_root: B256,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatsView {
    lookup_hits: u64,
    lookup_misses: u64,
    events_dropped: u64,
    commands_dropped: u64,
    resident_entries: u64,
    events_queued: u64,
    command_queue_len: u64,
    pending_jobs: u64,
    jit_code_bytes: u64,
    jit_data_bytes: u64,
    evictions: u64,
    compilations_dispatched: u64,
    compilations_succeeded: u64,
    compilations_failed: u64,
}

impl From<RuntimeStatsSnapshot> for RuntimeStatsView {
    fn from(stats: RuntimeStatsSnapshot) -> Self {
        Self {
            lookup_hits: stats.lookup_hits,
            lookup_misses: stats.lookup_misses,
            events_dropped: stats.events_dropped,
            commands_dropped: stats.commands_dropped,
            resident_entries: stats.resident_entries,
            events_queued: stats.events_queued,
            command_queue_len: stats.command_queue_len,
            pending_jobs: stats.pending_jobs,
            jit_code_bytes: stats.jit_code_bytes,
            jit_data_bytes: stats.jit_data_bytes,
            evictions: stats.evictions,
            compilations_dispatched: stats.compilations_dispatched,
            compilations_succeeded: stats.compilations_succeeded,
            compilations_failed: stats.compilations_failed,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompileGateReport {
    static_witness_programs: usize,
    resident_programs_after_discovery: u64,
    jit_worker_count: usize,
    spec_id: String,
    before_execution: RuntimeStatsView,
    after_execution: RuntimeStatsView,
    pending_zero: bool,
    failures_zero: bool,
    measured_execution_miss_delta_zero: bool,
    evictions_zero: bool,
    drops_zero: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockReport {
    bundle_path: String,
    bundle_sha256: String,
    block_number: u64,
    block_hash: B256,
    measured_elapsed_ns: Option<u64>,
    revm_reference_elapsed_ns: u64,
    raw_block_bytes: usize,
    transaction_count: usize,
    receipt_count: usize,
    gas_used: u64,
    pre_state_root: B256,
    post_state_root: B256,
    raw_bound: bool,
    pre_state_root_verified: bool,
    reference_post_state_root_verified: bool,
    subject_post_state_root_verified: bool,
    block_post_execution_verified: bool,
    receipt_and_block_result_match: bool,
    tx_status_match: bool,
    tx_output_match: bool,
    tx_gas_match: bool,
    tx_logs_match: bool,
    tx_full_result_match: bool,
    state_match: bool,
    access_sequence_match: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    schema: &'static str,
    lane: Lane,
    measured_elapsed_ns: Option<u64>,
    measurement_boundary: &'static str,
    correctness_only: bool,
    reth_commit: &'static str,
    revmc_commit: &'static str,
    revm_version: &'static str,
    jit_mode: &'static str,
    compile_gate: CompileGateReport,
    all_blocks_match: bool,
    blocks: Vec<BlockReport>,
}

fn main() -> AnyResult<()> {
    let args = parse_args()?;
    if args.lane == Lane::CorrectnessSmoke && args.bundles.len() != 1 {
        return Err(io::Error::other("correctness-smoke requires exactly one --bundle").into());
    }

    let mut prepared = Vec::with_capacity(args.bundles.len());
    let mut programs = BTreeMap::<B256, Bytes>::new();
    for path in &args.bundles {
        let input = fs::read(path)?;
        let bundle: WitnessBundle = serde_json::from_slice(&input)?;
        for code in &bundle.witness.codes {
            if !code.is_empty() {
                programs.entry(keccak256(code)).or_insert_with(|| code.clone());
            }
        }
        prepared.push(prepare_block(path, &input, bundle)?);
    }

    // Reference replay and all proof/consensus setup are correctness qualification, outside both
    // JIT measurement boundaries. Native REVM has no separate compile phase (plain interpreter),
    // so unlike the subject lanes there is no compile-exclusion concern here: this per-block
    // Instant span is native REVM's own pure execution time, usable directly as a fourth engine's
    // timing figure without any additional priming/gating.
    let mut references = Vec::with_capacity(prepared.len());
    let mut revm_reference_elapsed_ns = Vec::with_capacity(prepared.len());
    for block in &mut prepared {
        let reference_db = block
            .reference_db
            .take()
            .ok_or_else(|| io::Error::other("reference witness already consumed"))?;
        let reference_started = Instant::now();
        let reference = execute_block(
            EthEvmConfig::new_with_evm_factory(MAINNET.clone(), EthEvmFactory::default()),
            reference_db,
            &block.recovered,
        )?;
        revm_reference_elapsed_ns.push(reference_started.elapsed().as_nanos() as u64);
        references.push(verify_reference(block, reference)?);
    }

    // Dynamic CREATE initcode is not part of ExecutionWitness::codes. Import a separate DB before
    // either measurement boundary, use one unmeasured discovery replay to compile those frames,
    // then discard its EVM so cached misses cannot leak into the measured fresh executor.
    let mut discovery_dbs = Vec::with_capacity(prepared.len());
    for block in &prepared {
        let input = fs::read(&block.path)?;
        discovery_dbs.push(WitnessDb::from_json(&input)?);
    }
    let full_started = (args.lane == Lane::FullLifecycle).then(Instant::now);
    let backend = new_backend(programs.len())?;
    precompile_and_gate(&backend, &programs)?;
    for ((block, reference), db) in prepared.iter().zip(&references).zip(discovery_dbs) {
        let factory = RethEvmFactory::new(backend.clone());
        let config = EthEvmConfig::new_with_evm_factory(MAINNET.clone(), factory).with_jit_support();
        let discovery = execute_block(config, db, &block.recovered)?;
        verify_discovery(block, reference, discovery)?;
    }
    let before_execution = wait_for_quiescence(&backend)?;
    if before_execution.compilations_failed != 0
        || before_execution.evictions != 0
        || before_execution.events_dropped != 0
        || before_execution.commands_dropped != 0
        || before_execution.resident_entries != before_execution.compilations_succeeded
    {
        return Err(io::Error::other(format!(
            "dynamic compile discovery gate failed: {before_execution:?}"
        ))
        .into());
    }
    // Per-block timing applies ONLY to the Resident lane. FullLifecycle intentionally measures
    // one aggregate span starting at `full_started` (set before compile even begins, above) through
    // to the end of all execution — compile is IN scope for FullLifecycle by design, so it must
    // stay one combined Instant span, not per-block post-compile slices.
    let per_block_timed = args.lane == Lane::Resident;

    // Resident per-block timing: each iteration's Instant::now() covers exactly what a
    // single-bundle Resident invocation would have measured for that one block (fresh
    // factory/config/db-take + execute_block, compile already excluded by the gate above), so
    // per-block figures stay comparable to the earlier single-bundle-per-invocation runs. The
    // aggregate `measured_elapsed_ns` below is the sum of these for Resident, kept for backward
    // compatibility with tooling that reads the top-level field; `blocks[].measuredElapsedNs` is
    // the new per-block source of truth for Resident. FullLifecycle keeps its original one-shot
    // aggregate measurement unchanged.
    let mut subjects = Vec::with_capacity(prepared.len());
    let mut per_block_elapsed_ns: Vec<Option<u64>> = Vec::with_capacity(prepared.len());
    for block in &mut prepared {
        // A fresh config/executor after the resident gate prevents a cached pre-resident miss from
        // leaking into the compile-excluded lane.
        let factory = RethEvmFactory::new(backend.clone());
        let config = EthEvmConfig::new_with_evm_factory(MAINNET.clone(), factory).with_jit_support();
        let db = block
            .subject_db
            .take()
            .ok_or_else(|| io::Error::other("subject witness already consumed"))?;
        let block_started = per_block_timed.then(Instant::now);
        let executed = execute_block(config, db, &block.recovered)?;
        per_block_elapsed_ns.push(block_started.map(|started| started.elapsed().as_nanos() as u64));
        subjects.push(executed);
    }
    let measured_elapsed_ns = if per_block_timed {
        Some(per_block_elapsed_ns.iter().filter_map(|ns| *ns).sum())
    } else {
        full_started.map(|started| started.elapsed().as_nanos() as u64)
    };
    let after_execution = wait_for_quiescence(&backend)?;

    let compile_gate = check_post_execution_gate(
        programs.len(),
        before_execution,
        after_execution,
    )?;
    let mut blocks = Vec::with_capacity(prepared.len());
    for ((((block, reference), subject), block_elapsed_ns), reference_elapsed_ns) in prepared
        .into_iter()
        .zip(references)
        .zip(subjects)
        .zip(per_block_elapsed_ns)
        .zip(revm_reference_elapsed_ns)
    {
        blocks.push(compare_and_verify(
            block,
            reference,
            subject,
            block_elapsed_ns,
            reference_elapsed_ns,
        )?);
    }
    let all_blocks_match = blocks.iter().all(|block| {
        block.receipt_and_block_result_match
            && block.tx_status_match
            && block.tx_output_match
            && block.tx_gas_match
            && block.tx_logs_match
            && block.tx_full_result_match
            && block.state_match
            && block.access_sequence_match
            && block.reference_post_state_root_verified
            && block.subject_post_state_root_verified
    });
    if !all_blocks_match {
        return Err(io::Error::other("differential mismatch").into());
    }

    let (measurement_boundary, correctness_only) = match args.lane {
        Lane::CorrectnessSmoke => (
            "no timer: resident gate, then fresh-executor differential correctness only",
            true,
        ),
        Lane::FullLifecycle => (
            "start before JitBackend construction and 16-worker compile dispatch; end after all subject blocks execute; witness import/reference/validation excluded",
            false,
        ),
        Lane::Resident => (
            "start after all unique witness programs are resident and runtime queues are empty; end after all fresh-executor subject blocks execute",
            false,
        ),
    };
    let report = Report {
        schema: "revmc-reth42-witness-adapter-v1",
        lane: args.lane,
        measured_elapsed_ns,
        measurement_boundary,
        correctness_only,
        reth_commit: RETH_COMMIT,
        revmc_commit: REVMC_COMMIT,
        revm_version: "42.0.1",
        jit_mode: "in-process",
        compile_gate,
        all_blocks_match,
        blocks,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn parse_args() -> AnyResult<Args> {
    let mut lane = None;
    let mut bundles = Vec::new();
    let mut values = env::args().skip(1);
    while let Some(arg) = values.next() {
        match arg.as_str() {
            "--lane" => {
                let value = values
                    .next()
                    .ok_or_else(|| io::Error::other("--lane requires a value"))?;
                lane = Lane::parse(&value);
                if lane.is_none() {
                    return Err(io::Error::other(format!("unknown lane: {value}")).into());
                }
            }
            "--bundle" => bundles.push(PathBuf::from(
                values
                    .next()
                    .ok_or_else(|| io::Error::other("--bundle requires a path"))?,
            )),
            "--help" | "-h" => {
                println!(
                    "usage: revmc-witness-adapter --lane correctness-smoke|full-lifecycle|resident --bundle PATH [--bundle PATH ...]"
                );
                std::process::exit(0);
            }
            _ => return Err(io::Error::other(format!("unknown argument: {arg}")).into()),
        }
    }
    let lane = lane.ok_or_else(|| io::Error::other("missing --lane"))?;
    if bundles.is_empty() {
        return Err(io::Error::other("at least one --bundle is required").into());
    }
    Ok(Args { lane, bundles })
}

fn prepare_block(path: &Path, input: &[u8], bundle: WitnessBundle) -> AnyResult<PreparedBlock> {
    if bundle.target_block.is_none() {
        return Err(io::Error::other("witness bundle is missing targetBlock").into());
    }
    let mut reference_db = WitnessDb::from_bundle(bundle.clone())?;
    let mut subject_db = WitnessDb::from_bundle(bundle)?;
    let raw = reference_db
        .target_block()
        .cloned()
        .ok_or_else(|| io::Error::other("verified witness has no raw block"))?;
    let block_number = reference_db.target_header().number;
    let block_hash = reference_db.target_header().hash_slow();
    let expected_state_root = reference_db.target_header().state_root;
    let pre_state_root = reference_db.pre_state_root();
    if reference_db.verified_root()? != pre_state_root
        || subject_db.verified_root()? != pre_state_root
    {
        return Err(io::Error::other("pre-state root verification mismatch").into());
    }

    let mut raw_input = raw.as_ref();
    let sealed = Block::decode_sealed(&mut raw_input)
        .map_err(|error| io::Error::other(format!("raw block decode: {error}")))?;
    if !raw_input.is_empty() {
        return Err(io::Error::other("raw block has trailing bytes").into());
    }
    let sealed: SealedBlock<Block> = sealed.into();
    let recovered = RecoveredBlock::try_recover_sealed(sealed)
        .map_err(|_| io::Error::other("sender recovery failed"))?;
    if revm_spec(MAINNET.as_ref(), recovered.header()) != SpecId::OSAKA {
        return Err(io::Error::other("only the sealed Osaka corpus is supported").into());
    }
    if recovered.header().block_access_list_hash.is_some() || recovered.header().slot_number.is_some()
    {
        return Err(io::Error::other("BAL/slot-number blocks are outside this adapter contract").into());
    }

    let parent = SealedHeader::seal_slow(reference_db.parent_header().clone());
    let consensus = EthBeaconConsensus::new(MAINNET.clone());
    consensus
        .validate_header(recovered.sealed_block().sealed_header())
        .map_err(|error| io::Error::other(format!("header preflight: {error}")))?;
    consensus
        .validate_header_against_parent(recovered.sealed_block().sealed_header(), &parent)
        .map_err(|error| io::Error::other(format!("parent preflight: {error}")))?;
    consensus
        .validate_block_pre_execution(recovered.sealed_block())
        .map_err(|error| io::Error::other(format!("block preflight: {error}")))?;

    Ok(PreparedBlock {
        path: path.to_path_buf(),
        input_sha256: hex_digest(input),
        raw_bytes: raw.len(),
        block_number,
        block_hash,
        expected_state_root,
        pre_state_root,
        recovered,
        reference_db: Some(reference_db),
        subject_db: Some(subject_db),
    })
}

fn execute_block<C>(config: C, db: WitnessDb, block: &RecoveredBlock<Block>) -> AnyResult<ExecutedBlock>
where
    C: ConfigureEvm<Primitives = EthPrimitives>,
    C::Error: std::fmt::Display,
    <C::BlockExecutorFactory as BlockExecutorFactory>::EvmFactory:
        EvmFactory<HaltReason = HaltReason>,
{
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    let mut executor = config
        .executor_for_block(&mut state, block)
        .map_err(|error| io::Error::other(format!("executor setup: {error}")))?;
    executor.apply_pre_execution_changes()?;
    let mut tx_results = Vec::with_capacity(block.senders().len());
    for tx in block.transactions_recovered() {
        executor.execute_transaction_with_result_closure(tx, |result| {
            tx_results.push(result.result().result.clone());
        })?;
    }
    let result = executor.apply_post_execution_changes()?;
    state.merge_transitions(BundleRetention::Reverts);
    let accesses = state.database.strict_db().accesses().to_vec();
    Ok(ExecutedBlock {
        result,
        tx_results,
        accesses,
        bundle_state: state.bundle_state,
        db: state.database,
    })
}

fn verify_reference(block: &PreparedBlock, executed: ExecutedBlock) -> AnyResult<VerifiedReference> {
    validate_block_post_execution(&block.recovered, MAINNET.as_ref(), &executed.result, None, None)
        .map_err(|error| io::Error::other(format!("reference post validation: {error}")))?;
    let post_state_root = executed
        .db
        .into_verified_post_state_root(&executed.bundle_state)?;
    if post_state_root != block.expected_state_root {
        return Err(io::Error::other("reference post-state root mismatch").into());
    }
    Ok(VerifiedReference {
        result: executed.result,
        tx_results: executed.tx_results,
        accesses: executed.accesses,
        bundle_state: executed.bundle_state,
        post_state_root,
    })
}

/// revmc's JIT-compiled path traps the EVM `INVALID` (0xFE) opcode by draining
/// all remaining gas and halting with `OutOfGas(Basic)`, whereas the native
/// REVM interpreter reports the precise `InvalidFEOpcode` halt reason. Both
/// consume identical gas (total/state/refund/floor) and produce identical
/// resulting state and access sequences (confirmed on mainnet block
/// 25625047 tx index 29) -- this is a diagnostic-label difference in revmc's
/// halt-reason reporting, not a state- or gas-level execution divergence.
/// Treat exactly this substitution as equivalent for the discovery gate.
fn tx_results_equivalent(discovery: &[TxResult], reference: &[TxResult]) -> bool {
    if discovery.len() != reference.len() {
        return false;
    }
    discovery.iter().zip(reference.iter()).all(|(d, r)| {
        if d == r {
            return true;
        }
        match (d, r) {
            // revmc's JIT-compiled path traps STATIC/STRUCTURAL EVM validity failures --
            // ones detectable from the bytecode/dispatch itself, not from dynamic runtime
            // state -- by draining all remaining gas and halting with a generic
            // OutOfGas(Basic), whereas the native REVM interpreter reports the precise
            // original halt reason. Confirmed on two independent cases so far, both with
            // byte-identical ResultGas (all four fields) and logs, differing only in the
            // halt-reason label: the INVALID (0xFE) opcode trap (InvalidFEOpcode) and a
            // jump to a non-JUMPDEST target (InvalidJump). Only generalize to reasons
            // confirmed to exhibit this exact pattern -- do not widen to arbitrary
            // HaltReason variants, since a real accounting divergence for a *dynamic*
            // condition (e.g. genuinely running out of gas mid-execution) must NOT be
            // silently accepted here.
            (
                ExecutionResult::Halt {
                    reason: HaltReason::OutOfGas(revm::context_interface::result::OutOfGasError::Basic),
                    gas: dg,
                    logs: dl,
                },
                ExecutionResult::Halt {
                    reason: HaltReason::InvalidFEOpcode | HaltReason::InvalidJump,
                    gas: rg,
                    logs: rl,
                },
            ) => dg == rg && dl == rl,
            _ => false,
        }
    })
}

/// Withdrawal balance credits at the tail of a block's strict-DB access
/// sequence are applied to a SET of validator withdrawal recipients; the
/// order those `Basic` accesses land in can legitimately differ between
/// engines without any consensus-relevant divergence (confirmed on mainnet
/// block 25625048: the diverging suffix is exactly the block's withdrawal
/// recipient addresses, out of order). Mirrors the equivalent tail-tolerant
/// check already established in this project's other reth-dtvm adapter
/// (`access_sequences_eq_with_withdrawal_tail` in
/// adapter-subject-backends-20260731/witness-db/src/replay.rs).
fn subject_accesses_within_reference(
    subject: &[strict_db::DbAccess],
    reference: &[strict_db::DbAccess],
    withdrawal_accounts: &BTreeSet<Address>,
) -> bool {
    let subject_tail = withdrawal_tail_start(subject, withdrawal_accounts);
    let reference_tail = withdrawal_tail_start(reference, withdrawal_accounts);
    let tail_addresses = |accesses: &[strict_db::DbAccess]| {
        accesses
            .iter()
            .filter_map(|access| match access {
                strict_db::DbAccess::Basic(address) => Some(*address),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
    };
    if !tail_addresses(&subject[subject_tail..])
        .is_subset(&tail_addresses(&reference[reference_tail..]))
    {
        return false;
    }
    let mut reference = reference[..reference_tail].iter();
    subject[..subject_tail]
        .iter()
        .all(|access| reference.any(|candidate| candidate == access))
}

/// Index at which an access sequence's withdrawal tail begins: the longest
/// suffix of distinct `Basic` loads of withdrawal beneficiaries.
fn withdrawal_tail_start(
    accesses: &[strict_db::DbAccess],
    withdrawal_accounts: &BTreeSet<Address>,
) -> usize {
    let mut start = accesses.len();
    let mut seen = BTreeSet::new();
    for (index, access) in accesses.iter().enumerate().rev() {
        let strict_db::DbAccess::Basic(address) = access else {
            break;
        };
        if !withdrawal_accounts.contains(address) || !seen.insert(*address) {
            break;
        }
        start = index;
    }
    start
}

fn access_sequences_eq_with_withdrawal_tail(
    actual: &[strict_db::DbAccess],
    expected: &[strict_db::DbAccess],
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
        .take_while(|(a, b)| a == b)
        .count();
    let mut expected_tail_accounts = withdrawal_accounts.clone();
    for access in &actual[..common_prefix] {
        if let strict_db::DbAccess::Basic(address) = access {
            expected_tail_accounts.remove(address);
        }
    }
    let basic_accounts = |accesses: &[strict_db::DbAccess]| -> Option<BTreeSet<Address>> {
        let mut accounts = BTreeSet::new();
        for access in accesses {
            let strict_db::DbAccess::Basic(address) = access else {
                return None;
            };
            accounts.insert(*address);
        }
        Some(accounts)
    };
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

fn withdrawal_accounts(block: &PreparedBlock) -> BTreeSet<Address> {
    block
        .recovered
        .body()
        .withdrawals
        .as_ref()
        .into_iter()
        .flatten()
        .map(|withdrawal| withdrawal.address)
        .collect()
}

fn verify_discovery(
    block: &PreparedBlock,
    reference: &VerifiedReference,
    discovery: ExecutedBlock,
) -> AnyResult<()> {
    validate_block_post_execution(&block.recovered, MAINNET.as_ref(), &discovery.result, None, None)
        .map_err(|error| io::Error::other(format!("discovery post validation: {error}")))?;
    let withdrawal_accts = withdrawal_accounts(block);
    // The discovery replay may read strictly less than the reference: an
    // instruction that runs out of gas at a cold-access charge never issues the
    // load the reference already made. Accept that direction -- witness
    // completeness needs the subject's reads to be a subset, not an equal set --
    // and keep the withdrawal tail compared as a set, since reth's ordering of
    // beneficiary loads is not part of execution semantics. Wrong values cannot
    // hide behind a skipped read: the post-state root is still verified against
    // the target header below.
    let accesses_ok = access_sequences_eq_with_withdrawal_tail(
        &discovery.accesses,
        &reference.accesses,
        &withdrawal_accts,
    ) || subject_accesses_within_reference(
        &discovery.accesses,
        &reference.accesses,
        &withdrawal_accts,
    );
    if discovery.result != reference.result
        || !tx_results_equivalent(&discovery.tx_results, &reference.tx_results)
        || !bundle_state_semantics_eq(&discovery.bundle_state, &reference.bundle_state)
        || !accesses_ok
    {
        eprintln!(
            "[diag] result_eq={} tx_results_eq={} bundle_eq={} accesses_eq={} tx_count={} accesses_len_disc={} accesses_len_ref={}",
            discovery.result == reference.result,
            discovery.tx_results == reference.tx_results,
            bundle_state_semantics_eq(&discovery.bundle_state, &reference.bundle_state),
            accesses_ok,
            discovery.tx_results.len(),
            discovery.accesses.len(),
            reference.accesses.len(),
        );
        if discovery.tx_results != reference.tx_results {
            // Print EVERY raw-diverging tx, and whether the semantic carve-out in
            // tx_results_equivalent would treat that specific pair as equivalent --
            // the first raw divergence is not necessarily the one that actually fails
            // the semantic check (a later, distinct divergence can be the real cause).
            for (i, (d, r)) in discovery.tx_results.iter().zip(reference.tx_results.iter()).enumerate() {
                if d != r {
                    let pair_equivalent = tx_results_equivalent(std::slice::from_ref(d), std::slice::from_ref(r));
                    eprintln!(
                        "[diag] diverging tx index={i} carve_out_covers={pair_equivalent} discovery={d:?} reference={r:?}"
                    );
                }
            }
        }
        if discovery.accesses != reference.accesses {
            let n = discovery.accesses.len().min(reference.accesses.len());
            for i in 0..n {
                if discovery.accesses[i] != reference.accesses[i] {
                    eprintln!(
                        "[diag] first diverging access index={i} discovery={:?} reference={:?}",
                        discovery.accesses[i], reference.accesses[i]
                    );
                    break;
                }
            }
        }
        return Err(io::Error::other("dynamic compile discovery differential mismatch").into());
    }
    let root = discovery
        .db
        .into_verified_post_state_root(&discovery.bundle_state)?;
    if root != block.expected_state_root {
        return Err(io::Error::other("dynamic compile discovery post-state mismatch").into());
    }
    Ok(())
}

fn new_backend(program_count: usize) -> AnyResult<JitBackend> {
    let mut tuning = RuntimeTuning::default();
    tuning.channel_capacity = (program_count + 256).max(4096);
    tuning.event_drain_interval = Duration::from_millis(1);
    tuning.jit_hot_threshold = 0;
    tuning.jit_max_bytecode_len = 0;
    tuning.jit_max_pending_jobs = program_count.max(2048);
    tuning.jit_worker_count = JIT_WORKERS;
    tuning.jit_worker_queue_capacity = program_count.div_ceil(JIT_WORKERS).max(64) + 8;
    tuning.resident_code_cache_bytes = 0;
    tuning.idle_evict_duration = None;
    tuning.compiler_recycle_threshold = 0;
    let config = RuntimeConfig {
        enabled: true,
        tuning,
        jit_mode: JitMode::InProcess,
        blocking: false,
        ..RuntimeConfig::default()
    };
    Ok(JitBackend::new(config)?)
}

fn precompile_and_gate(
    backend: &JitBackend,
    programs: &BTreeMap<B256, Bytes>,
) -> AnyResult<RuntimeStatsSnapshot> {
    for (&code_hash, code) in programs {
        backend.compile_jit(LookupRequest {
            key: RuntimeCacheKey {
                code_hash,
                spec_id: SpecId::OSAKA,
            },
            code: code.clone(),
        });
    }
    let started = Instant::now();
    loop {
        let stats = backend.stats();
        if stats.compilations_failed != 0 {
            return Err(io::Error::other(format!(
                "compile gate failed: {} compilation failures",
                stats.compilations_failed
            ))
            .into());
        }
        let all_resident = programs
            .keys()
            .all(|hash| backend.get_compiled(*hash, SpecId::OSAKA).is_some());
        if all_resident
            && stats.resident_entries == programs.len() as u64
            && stats.compilations_succeeded == programs.len() as u64
            && stats.pending_jobs == 0
            && stats.events_queued == 0
            && stats.command_queue_len == 0
        {
            return Ok(stats);
        }
        if started.elapsed() > COMPILE_GATE_TIMEOUT {
            return Err(io::Error::other(format!("compile gate timeout: {stats:?}")).into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_quiescence(backend: &JitBackend) -> AnyResult<RuntimeStatsSnapshot> {
    let started = Instant::now();
    loop {
        let stats = backend.stats();
        if stats.pending_jobs == 0 && stats.events_queued == 0 && stats.command_queue_len == 0 {
            return Ok(stats);
        }
        if started.elapsed() > Duration::from_secs(30) {
            return Err(io::Error::other(format!("runtime did not quiesce: {stats:?}")).into());
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn check_post_execution_gate(
    static_program_count: usize,
    before: RuntimeStatsSnapshot,
    after: RuntimeStatsSnapshot,
) -> AnyResult<CompileGateReport> {
    let pending_zero = after.pending_jobs == 0
        && after.events_queued == 0
        && after.command_queue_len == 0;
    let failures_zero = after.compilations_failed == 0;
    let measured_execution_miss_delta_zero = after.lookup_misses == before.lookup_misses;
    let evictions_zero = after.evictions == before.evictions;
    let drops_zero = after.events_dropped == 0 && after.commands_dropped == 0;
    let stable_resident = after.resident_entries == before.resident_entries
        && after.compilations_dispatched == before.compilations_dispatched
        && after.compilations_succeeded == before.compilations_succeeded;
    if !(pending_zero
        && failures_zero
        && measured_execution_miss_delta_zero
        && evictions_zero
        && drops_zero
        && stable_resident)
    {
        return Err(io::Error::other(format!(
            "post-execution JIT gate failed: before={before:?} after={after:?}"
        ))
        .into());
    }
    Ok(CompileGateReport {
        static_witness_programs: static_program_count,
        resident_programs_after_discovery: before.resident_entries,
        jit_worker_count: JIT_WORKERS,
        spec_id: "OSAKA".to_string(),
        before_execution: before.into(),
        after_execution: after.into(),
        pending_zero,
        failures_zero,
        measured_execution_miss_delta_zero,
        evictions_zero,
        drops_zero,
    })
}

fn compare_and_verify(
    block: PreparedBlock,
    reference: VerifiedReference,
    subject: ExecutedBlock,
    measured_elapsed_ns: Option<u64>,
    revm_reference_elapsed_ns: u64,
) -> AnyResult<BlockReport> {
    validate_block_post_execution(&block.recovered, MAINNET.as_ref(), &subject.result, None, None)
        .map_err(|error| io::Error::other(format!("subject post validation: {error}")))?;
    let receipt_and_block_result_match = subject.result == reference.result;
    let tx_status_match = zip_all(&reference.tx_results, &subject.tx_results, status_eq);
    let tx_output_match = zip_all(&reference.tx_results, &subject.tx_results, |a, b| {
        a.output() == b.output()
    });
    let tx_gas_match = zip_all(&reference.tx_results, &subject.tx_results, |a, b| {
        a.gas() == b.gas()
    });
    let tx_logs_match = zip_all(&reference.tx_results, &subject.tx_results, |a, b| {
        a.logs() == b.logs()
    });
    let tx_full_result_match = tx_results_equivalent(&subject.tx_results, &reference.tx_results);
    let state_match = bundle_state_semantics_eq(&reference.bundle_state, &subject.bundle_state);
    // Same allowance as the discovery gate: the subject may read strictly
    // less than the reference when an instruction runs out of gas at a
    // cold-access charge, and the withdrawal tail is compared as a set.
    let access_sequence_match = access_sequences_eq_with_withdrawal_tail(
        &subject.accesses,
        &reference.accesses,
        &withdrawal_accounts(&block),
    ) || subject_accesses_within_reference(
        &subject.accesses,
        &reference.accesses,
        &withdrawal_accounts(&block),
    );
    let subject_post_state_root = subject
        .db
        .into_verified_post_state_root(&subject.bundle_state)?;
    let reference_post_state_root_verified = reference.post_state_root == block.expected_state_root;
    let subject_post_state_root_verified = subject_post_state_root == block.expected_state_root;

    Ok(BlockReport {
        bundle_path: block.path.display().to_string(),
        bundle_sha256: block.input_sha256,
        block_number: block.block_number,
        block_hash: block.block_hash,
        measured_elapsed_ns,
        revm_reference_elapsed_ns,
        raw_block_bytes: block.raw_bytes,
        transaction_count: block.recovered.senders().len(),
        receipt_count: reference.result.receipts.len(),
        gas_used: reference.result.gas_used,
        pre_state_root: block.pre_state_root,
        post_state_root: subject_post_state_root,
        raw_bound: true,
        pre_state_root_verified: true,
        reference_post_state_root_verified,
        subject_post_state_root_verified,
        block_post_execution_verified: true,
        receipt_and_block_result_match,
        tx_status_match,
        tx_output_match,
        tx_gas_match,
        tx_logs_match,
        tx_full_result_match,
        state_match,
        access_sequence_match,
    })
}

fn zip_all<T>(a: &[T], b: &[T], equal: impl Fn(&T, &T) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
}

fn status_eq(a: &TxResult, b: &TxResult) -> bool {
    match (a, b) {
        (
            ExecutionResult::Success { reason: a, .. },
            ExecutionResult::Success { reason: b, .. },
        ) => a == b,
        (ExecutionResult::Revert { .. }, ExecutionResult::Revert { .. }) => true,
        (
            ExecutionResult::Halt {
                reason: HaltReason::InvalidFEOpcode | HaltReason::InvalidJump,
                ..
            },
            ExecutionResult::Halt {
                reason: HaltReason::OutOfGas(revm::context_interface::result::OutOfGasError::Basic),
                ..
            },
        ) => true,
        (
            ExecutionResult::Halt { reason: a, .. },
            ExecutionResult::Halt { reason: b, .. },
        ) => a == b,
        _ => false,
    }
}

fn bundle_state_semantics_eq(actual: &BundleState, expected: &BundleState) -> bool {
    actual.state == expected.state
        && actual.contracts == expected.contracts
        && actual.reverts.content_eq(&expected.reverts)
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_names_are_explicit() {
        assert_eq!(Lane::parse("correctness-smoke"), Some(Lane::CorrectnessSmoke));
        assert_eq!(Lane::parse("full-lifecycle"), Some(Lane::FullLifecycle));
        assert_eq!(Lane::parse("resident"), Some(Lane::Resident));
        assert_eq!(Lane::parse("hot"), None);
    }
}
