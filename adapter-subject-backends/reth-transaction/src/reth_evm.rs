//! `alloy_evm::EvmFactory` adapter for Osaka transactions and system calls backed by an
//! allowlisted EVMC subject.

use crate::journal_host::JournalBackend;
use alloy_evm::{
    env::EvmEnv,
    eth::{EthEvmBuilder, EthEvmContext},
    evm::EvmFactory,
    precompiles::PrecompilesMap,
    Database, Evm as AlloyEvm,
};
use alloy_primitives::{Address, Bytes, U256};
use reth_dtvm_adapter::{
    ffi::{
        EvmcAddress, EvmcBytes32, EvmcTxContext, EVMC_ARGUMENT_OUT_OF_RANGE,
        EVMC_BAD_JUMP_DESTINATION, EVMC_CALL_DEPTH_EXCEEDED, EVMC_DELEGATED,
        EVMC_INSUFFICIENT_BALANCE, EVMC_INVALID_INSTRUCTION, EVMC_INVALID_MEMORY_ACCESS,
        EVMC_OSAKA, EVMC_OUT_OF_GAS, EVMC_PRECOMPILE_FAILURE, EVMC_STACK_OVERFLOW,
        EVMC_STACK_UNDERFLOW, EVMC_UNDEFINED_INSTRUCTION,
    },
    AccessEvent, Address as DtvmAddress, Dtvm, DtvmEvmcHotMetrics, DtvmPhaseMetricsCollector,
    DtvmPhaseMetricsReport, EvmoneAdvancedDiagnosticMetrics, FrameStatus, HostContext, Message,
    SubjectBackend, TxContextOwned, Word,
};
use revm::{
    context::{
        result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction, ResultAndState},
        BlockEnv, CfgEnv, Context, DBErrorMarker, Evm as RevmEvm, TxEnv,
    },
    context_interface::{context::take_error, ContextSetters, JournalTr},
    handler::{
        instructions::EthInstructions, EthFrame, EvmTr, ExecuteEvm, FrameInitOrResult, FrameTr,
        Handler, ItemOrResult, MainnetHandler, SystemCallTx,
    },
    inspector::{Inspector, NoOpInspector},
    interpreter::{
        interpreter::EthInterpreter, Gas, InstructionResult, InterpreterAction, InterpreterResult,
    },
    primitives::hardfork::SpecId,
    state::EvmState,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Original transaction-differential sample retained as a regression corpus.
pub const STORAGE_LOG_RETURN: &[u8] = &[
    0x60, 0x2a, 0x5f, 0x55, 0x60, 0x2a, 0x5f, 0x52, 0x60, 0x01, 0x60, 0x20, 0x5f, 0xa1, 0x5f, 0x54,
    0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
];

/// Gas limit used by the original transaction-differential sample.
pub const SUPPORTED_TX_GAS_LIMIT: u64 = 1_021_000;

type InnerEvm<DB, I> = RevmEvm<
    EthEvmContext<DB>,
    I,
    EthInstructions<EthInterpreter, EthEvmContext<DB>>,
    PrecompilesMap,
    EthFrame,
>;

type SharedDtvm = Rc<RefCell<Result<Dtvm, String>>>;

thread_local! {
    static THREAD_DTVM: RefCell<HashMap<u64, SharedDtvm>> = RefCell::new(HashMap::new());
}

static NEXT_FACTORY_ID: AtomicU64 = AtomicU64::new(1);

/// One transaction-scoped EVMC subject wrapped in the current REVM handler.
pub struct DtvmEvm<DB: Database, I> {
    inner: InnerEvm<DB, I>,
    dtvm: SharedDtvm,
    inspect: bool,
    last_audit: Vec<AccessEvent>,
}

impl<DB: Database, I> fmt::Debug for DtvmEvm<DB, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DtvmEvm")
            .field(
                "dtvm_loaded",
                &self.dtvm.try_borrow().is_ok_and(|dtvm| dtvm.is_ok()),
            )
            .field("inspect", &self.inspect)
            .field("last_audit", &self.last_audit)
            .finish_non_exhaustive()
    }
}

impl<DB: Database, I> DtvmEvm<DB, I> {
    fn new(inner: InnerEvm<DB, I>, inspect: bool, dtvm: SharedDtvm) -> Self {
        Self {
            inner,
            dtvm,
            inspect,
            last_audit: Vec::new(),
        }
    }

    /// Host callback attempts from the most recent EVMC subject frame.
    pub fn last_audit(&self) -> &[AccessEvent] {
        &self.last_audit
    }

    fn validate_transaction(&self, tx: &TxEnv) -> Result<(), EVMError<DB::Error>> {
        self.validate_environment()?;
        if !matches!(tx.tx_type, 0..=4) {
            return Err(evm_custom("unsupported Ethereum transaction type"));
        }
        Ok(())
    }

    fn validate_environment(&self) -> Result<(), EVMError<DB::Error>> {
        if self.inner.ctx.cfg.spec != SpecId::OSAKA {
            return Err(evm_custom("EVMC subject slice requires SpecId::OSAKA"));
        }
        if self.inner.ctx.cfg.chain_id != 1 {
            return Err(evm_custom(
                "EVMC subject slice requires Ethereum mainnet chain id 1",
            ));
        }
        Ok(())
    }

    fn ensure_dtvm_loaded(&self) -> Result<(), EVMError<DB::Error>> {
        let dtvm = self
            .dtvm
            .try_borrow()
            .map_err(|_| evm_custom("shared EVMC subject is already borrowed"))?;
        if let Err(error) = &*dtvm {
            return Err(evm_custom(format!("EVMC subject load failed: {error}")));
        }
        Ok(())
    }
}

impl<DB: Database, I> EvmTr for DtvmEvm<DB, I> {
    type Context = EthEvmContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, EthEvmContext<DB>>;
    type Precompiles = PrecompilesMap;
    type Frame = EthFrame;

    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &revm::context_interface::FrameStack<Self::Frame>,
    ) {
        self.inner.all()
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut revm::context_interface::FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<
        ItemOrResult<&mut Self::Frame, <Self::Frame as FrameTr>::FrameResult>,
        revm::handler::evm::ContextDbError<Self::Context>,
    > {
        self.inner.frame_init(frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, revm::handler::evm::ContextDbError<Self::Context>>
    {
        let Self {
            inner,
            dtvm,
            last_audit,
            ..
        } = self;
        let (ctx, _, precompiles, frame_stack) = inner.all_mut();
        let frame = frame_stack.get();

        if frame.depth != 0 {
            return Err(custom("nested frame reached the EVMC subject first slice"));
        }
        if frame.interpreter.gas.reservoir() != 0 {
            return Err(custom("EIP-8037 reservoir is unsupported"));
        }

        let frame_input = frame.input.clone();
        let code = frame.interpreter.bytecode.original_bytes();

        let (message, gas_limit) = match frame_input {
            revm::interpreter::FrameInput::Call(inputs) => {
                if inputs.reservoir != 0
                    || inputs.is_static
                    || inputs.scheme != revm::interpreter::CallScheme::Call
                {
                    return Err(custom("only a non-static top-level CALL is supported"));
                }

                let delegated_address = ctx
                    .journaled_state
                    .load_account_with_code(inputs.target_address)?
                    .info
                    .code
                    .as_ref()
                    .and_then(revm::bytecode::Bytecode::eip7702_address);
                let input = inputs.input.bytes(ctx);
                let mut message = Message::ordinary(
                    to_dtvm_address(inputs.target_address),
                    to_dtvm_address(inputs.caller),
                    input.to_vec(),
                    Word(inputs.call_value().to_be_bytes()),
                    inputs.gas_limit,
                );
                if let Some(delegated_address) = delegated_address {
                    message.code_address = to_dtvm_address(delegated_address);
                    message.flags = EVMC_DELEGATED;
                }
                (message, inputs.gas_limit)
            }
            revm::interpreter::FrameInput::Create(inputs) => {
                if inputs.reservoir() != 0
                    || inputs.scheme() != revm::context_interface::CreateScheme::Create
                {
                    return Err(custom("only top-level CREATE is supported"));
                }
                if code.as_ref() != inputs.init_code().as_ref() {
                    return Err(custom(
                        "top-level CREATE interpreter code differs from initcode",
                    ));
                }
                (
                    Message::create(
                        to_dtvm_address(frame.interpreter.input.target_address),
                        to_dtvm_address(inputs.caller()),
                        Word(inputs.value().to_be_bytes()),
                        inputs.gas_limit(),
                    ),
                    inputs.gas_limit(),
                )
            }
            revm::interpreter::FrameInput::Empty => {
                return Err(custom("empty top-level frame is unsupported"));
            }
        };
        let tx_context = tx_context(ctx)?;
        let mut dtvm = dtvm
            .try_borrow_mut()
            .map_err(|_| custom("shared EVMC subject is already borrowed"))?;
        let dtvm = dtvm
            .as_mut()
            .map_err(|error| custom(format!("EVMC subject load failed: {error}")))?;

        let execution = {
            let mut backend = JournalBackend::new(ctx, precompiles);
            let mut host = HostContext::new(&mut backend, tx_context);
            let execution = dtvm.execute(EVMC_OSAKA, &message, code.as_ref(), &mut host);
            *last_audit = host.audit();
            execution
        };

        let outcome = match execution {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Err(database_error) =
                    take_error::<revm::context::ContextError<DB::Error>, _>(&mut ctx.error)
                {
                    return Err(database_error);
                }
                return Err(custom(format!("EVMC subject execution failed: {error}")));
            }
        };
        let instruction_result = match outcome.status {
            // EVMC_SUCCESS does not encode whether the VM stopped via STOP or
            // RETURN. Until the bridge has an independent terminal-opcode
            // signal, preserve output and conservatively classify it as RETURN.
            FrameStatus::Success => InstructionResult::Return,
            FrameStatus::Revert => InstructionResult::Revert,
            FrameStatus::Halt(status) => map_evmc_halt(status)
                .ok_or_else(|| custom(format!("ambiguous EVMC halt status {status}")))?,
        };

        let mut gas = Gas::new(gas_limit);
        gas.set_remaining(outcome.gas_left);
        gas.set_refund(outcome.gas_refund);
        let action = InterpreterAction::Return(InterpreterResult {
            result: instruction_result,
            gas,
            output: Bytes::from(outcome.output),
        });
        let result =
            frame.process_next_action::<_, revm::context::ContextError<DB::Error>>(ctx, action)?;
        if result.is_result() {
            frame.set_finished(true);
        }
        Ok(result)
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as FrameTr>::FrameResult>,
        revm::handler::evm::ContextDbError<Self::Context>,
    > {
        self.inner.frame_return_result(result)
    }
}

impl<DB: Database, I> ExecuteEvm for DtvmEvm<DB, I> {
    type ExecutionResult = ExecutionResult<HaltReason>;
    type State = EvmState;
    type Error = EVMError<DB::Error, InvalidTransaction>;
    type Tx = TxEnv;
    type Block = BlockEnv;

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.last_audit.clear();
        if self.inspect {
            return Err(evm_custom(
                "inspector execution is fail-closed in this slice",
            ));
        }
        self.ensure_dtvm_loaded()?;
        self.validate_transaction(&tx)?;
        self.inner.ctx.set_tx(tx);
        MainnetHandler::default().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journaled_state.finalize()
    }

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn replay(&mut self) -> Result<ResultAndState<HaltReason>, Self::Error> {
        self.last_audit.clear();
        if self.inspect {
            return Err(evm_custom(
                "inspector execution is fail-closed in this slice",
            ));
        }
        self.ensure_dtvm_loaded()?;
        self.validate_transaction(&self.inner.ctx.tx)?;
        MainnetHandler::default().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, I> AlloyEvm for DtvmEvm<DB, I>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.inner.ctx.block
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        &self.inner.ctx.cfg
    }

    fn chain_id(&self) -> u64 {
        self.inner.ctx.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        ExecuteEvm::transact(self, tx)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.last_audit.clear();
        if self.inspect {
            return Err(evm_custom(
                "inspector execution is fail-closed in this slice",
            ));
        }
        self.ensure_dtvm_loaded()?;
        self.validate_environment()?;
        self.inner
            .ctx
            .set_tx(TxEnv::new_system_tx_with_caller(caller, contract, data));
        let output_or_error: Result<ExecutionResult<HaltReason>, EVMError<DB::Error>> =
            MainnetHandler::default().run_system_call(self);
        // Finalize on both paths so a failed host callback cannot leave a
        // partially loaded or modified system-call journal behind.
        let state = self.finalize();
        let result = output_or_error?;
        Ok(ResultAndState::new(result, state))
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context {
            block: block_env,
            cfg: cfg_env,
            journaled_state,
            ..
        } = self.inner.ctx;
        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (
            &self.inner.ctx.journaled_state.database,
            &self.inner.inspector,
            &self.inner.precompiles,
        )
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.ctx.journaled_state.database,
            &mut self.inner.inspector,
            &mut self.inner.precompiles,
        )
    }
}

/// Single-threaded factory retaining one EVMC VM and its code cache.
#[derive(Clone, Debug)]
pub struct DtvmEvmFactory {
    state: Arc<DtvmEvmFactoryState>,
}

#[derive(Debug)]
struct DtvmEvmFactoryState {
    id: u64,
    library_path: PathBuf,
    backend: Option<SubjectBackend>,
    phase_metrics: Option<DtvmPhaseMetricsCollector>,
    vm_create_count: AtomicU64,
}

impl Drop for DtvmEvmFactoryState {
    fn drop(&mut self) {
        let _ = THREAD_DTVM.try_with(|dtvms| dtvms.borrow_mut().remove(&self.id));
    }
}

impl DtvmEvmFactory {
    pub fn new(library_path: impl Into<PathBuf>) -> Self {
        Self {
            state: Arc::new(DtvmEvmFactoryState {
                id: NEXT_FACTORY_ID.fetch_add(1, Ordering::Relaxed),
                library_path: library_path.into(),
                backend: None,
                phase_metrics: None,
                vm_create_count: AtomicU64::new(0),
            }),
        }
    }

    pub fn new_for(library_path: impl Into<PathBuf>, backend: SubjectBackend) -> Self {
        Self {
            state: Arc::new(DtvmEvmFactoryState {
                id: NEXT_FACTORY_ID.fetch_add(1, Ordering::Relaxed),
                library_path: library_path.into(),
                backend: Some(backend),
                phase_metrics: (backend == SubjectBackend::DtvmEager)
                    .then(DtvmPhaseMetricsCollector::default),
                vm_create_count: AtomicU64::new(0),
            }),
        }
    }

    pub fn library_path(&self) -> &Path {
        &self.state.library_path
    }

    pub fn dtvm_phase_metrics(&self) -> Option<DtvmPhaseMetricsReport> {
        let collector = self.state.phase_metrics.as_ref()?;
        self.shared_dtvm()
            .try_borrow()
            .ok()
            .and_then(|dtvm| dtvm.as_ref().ok()?.phase_metrics_snapshot())
            .or_else(|| Some(collector.report()))
    }

    pub fn vm_create_count(&self) -> u64 {
        self.state.vm_create_count.load(Ordering::Relaxed)
    }

    pub fn require_hot_metrics_v2(&self) -> Result<(), String> {
        self.shared_dtvm()
            .try_borrow_mut()
            .map_err(|_| "shared EVMC subject is already borrowed".to_string())?
            .as_mut()
            .map_err(|error| error.clone())?
            .require_hot_metrics_v2()
            .map_err(|error| error.to_string())
    }

    pub fn hot_metrics_snapshot(&self) -> Result<DtvmEvmcHotMetrics, String> {
        self.shared_dtvm()
            .try_borrow()
            .map_err(|_| "shared EVMC subject is already borrowed".to_string())?
            .as_ref()
            .map_err(|error| error.clone())?
            .hot_metrics_snapshot()
            .map_err(|error| error.to_string())
    }

    pub fn require_evmone_diagnostic_metrics_v1(&self) -> Result<(), String> {
        self.shared_dtvm()
            .try_borrow_mut()
            .map_err(|_| "shared EVMC subject is already borrowed".to_string())?
            .as_mut()
            .map_err(|error| error.clone())?
            .require_evmone_diagnostic_metrics_v1()
            .map_err(|error| error.to_string())
    }

    pub fn evmone_diagnostic_metrics_snapshot(
        &self,
    ) -> Result<EvmoneAdvancedDiagnosticMetrics, String> {
        self.shared_dtvm()
            .try_borrow()
            .map_err(|_| "shared EVMC subject is already borrowed".to_string())?
            .as_ref()
            .map_err(|error| error.clone())?
            .evmone_diagnostic_metrics_snapshot()
            .map_err(|error| error.to_string())
    }

    fn shared_dtvm(&self) -> SharedDtvm {
        THREAD_DTVM.with(|dtvms| {
            if let Some(dtvm) = dtvms.borrow().get(&self.state.id).cloned() {
                return dtvm;
            }
            let dtvm = Rc::new(RefCell::new(load_dtvm(
                &self.state.library_path,
                self.state.backend,
                self.state.phase_metrics.as_ref(),
            )));
            self.state.vm_create_count.fetch_add(1, Ordering::Relaxed);
            dtvms.borrow_mut().insert(self.state.id, Rc::clone(&dtvm));
            dtvm
        })
    }
}

fn load_dtvm(
    library_path: &Path,
    backend: Option<SubjectBackend>,
    phase_metrics: Option<&DtvmPhaseMetricsCollector>,
) -> Result<Dtvm, String> {
    // SAFETY: the experiment pins and verifies this path before execution.
    unsafe {
        match (backend, phase_metrics) {
            (Some(SubjectBackend::DtvmEager), Some(collector)) => {
                Dtvm::load_for_with_phase_metrics(
                    library_path,
                    SubjectBackend::DtvmEager,
                    collector.clone(),
                )
            }
            (Some(backend), _) => Dtvm::load_for(library_path, backend),
            (None, _) => Dtvm::load(library_path),
        }
    }
    .map_err(|error| error.to_string())
}

pub type SubjectEvmFactory = DtvmEvmFactory;

impl EvmFactory for DtvmEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>>> = DtvmEvm<DB, I>;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        let inner = EthEvmBuilder::new(db, input).build().into_inner();
        DtvmEvm::new(inner, false, self.shared_dtvm())
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let inner = EthEvmBuilder::new(db, input)
            .activate_inspector(inspector)
            .build()
            .into_inner();
        DtvmEvm::new(inner, true, self.shared_dtvm())
    }
}

fn tx_context<DB: Database>(
    ctx: &EthEvmContext<DB>,
) -> Result<TxContextOwned, revm::context::ContextError<DB::Error>> {
    let block_number = i64_from_u256(ctx.block.number, "block number")?;
    let block_timestamp = i64_from_u256(ctx.block.timestamp, "block timestamp")?;
    let block_gas_limit = i64::try_from(ctx.block.gas_limit)
        .map_err(|_| custom("block gas limit exceeds EVMC int64"))?;
    let prevrandao = ctx
        .block
        .prevrandao
        .ok_or_else(|| custom("post-merge Osaka block requires prevrandao"))?;
    let blob_base_fee = ctx
        .block
        .blob_excess_gas_and_price
        .map(|value| U256::from(value.blob_gasprice))
        .unwrap_or_default();
    let effective_gas_price = U256::from(
        ctx.tx
            .gas_priority_fee
            .map(|priority| {
                ctx.tx
                    .gas_price
                    .min((ctx.block.basefee as u128).saturating_add(priority))
            })
            .unwrap_or(ctx.tx.gas_price),
    );

    let blob_hashes = ctx
        .tx
        .blob_hashes
        .iter()
        .map(|hash| EvmcBytes32 { bytes: hash.0 })
        .collect();

    Ok(TxContextOwned::new(
        EvmcTxContext {
            tx_gas_price: to_evmc_word(effective_gas_price),
            tx_origin: to_evmc_address(ctx.tx.caller),
            block_coinbase: to_evmc_address(ctx.block.beneficiary),
            block_number,
            block_timestamp,
            block_gas_limit,
            block_prev_randao: EvmcBytes32 {
                bytes: prevrandao.0,
            },
            chain_id: to_evmc_word(U256::from(ctx.cfg.chain_id)),
            block_base_fee: to_evmc_word(U256::from(ctx.block.basefee)),
            blob_base_fee: to_evmc_word(blob_base_fee),
            ..Default::default()
        },
        blob_hashes,
        Vec::new(),
    ))
}

fn map_evmc_halt(status: i32) -> Option<InstructionResult> {
    Some(match status {
        EVMC_INVALID_INSTRUCTION => InstructionResult::InvalidFEOpcode,
        EVMC_UNDEFINED_INSTRUCTION => InstructionResult::OpcodeNotFound,
        EVMC_STACK_OVERFLOW => InstructionResult::StackOverflow,
        EVMC_STACK_UNDERFLOW => InstructionResult::StackUnderflow,
        EVMC_BAD_JUMP_DESTINATION => InstructionResult::InvalidJump,
        EVMC_INVALID_MEMORY_ACCESS | EVMC_ARGUMENT_OUT_OF_RANGE => InstructionResult::OutOfOffset,
        EVMC_CALL_DEPTH_EXCEEDED => InstructionResult::CallTooDeep,
        EVMC_PRECOMPILE_FAILURE => InstructionResult::PrecompileError,
        EVMC_INSUFFICIENT_BALANCE => InstructionResult::OutOfFunds,
        // EVMC folds REVM's OOG subtypes into one consensus-equivalent
        // exceptional halt. Receipts and post-state do not encode the subtype.
        EVMC_OUT_OF_GAS => InstructionResult::OutOfGas,
        // EVMC does not distinguish the two REVM static violations or the
        // possible contract-validation failures.
        _ => return None,
    })
}

fn i64_from_u256<DBError>(
    value: U256,
    field: &'static str,
) -> Result<i64, revm::context::ContextError<DBError>> {
    let value = u64::try_from(value).map_err(|_| custom(format!("{field} exceeds u64")))?;
    i64::try_from(value).map_err(|_| custom(format!("{field} exceeds EVMC int64")))
}

fn to_dtvm_address(address: Address) -> DtvmAddress {
    DtvmAddress(address.0 .0)
}

fn to_evmc_address(address: Address) -> EvmcAddress {
    EvmcAddress {
        bytes: address.0 .0,
    }
}

fn to_evmc_word(value: U256) -> EvmcBytes32 {
    EvmcBytes32 {
        bytes: value.to_be_bytes(),
    }
}

fn custom<DBError>(message: impl Into<String>) -> revm::context::ContextError<DBError> {
    revm::context::ContextError::Custom(message.into())
}

fn evm_custom<DBError>(message: impl Into<String>) -> EVMError<DBError> {
    EVMError::Custom(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_dtvm_adapter::ffi::{EVMC_CONTRACT_VALIDATION_FAILURE, EVMC_STATIC_MODE_VIOLATION};

    #[test]
    fn unambiguous_evmc_halts_map_to_revm_instruction_results() {
        assert_eq!(
            map_evmc_halt(EVMC_INVALID_INSTRUCTION),
            Some(InstructionResult::InvalidFEOpcode)
        );
        assert_eq!(
            map_evmc_halt(EVMC_UNDEFINED_INSTRUCTION),
            Some(InstructionResult::OpcodeNotFound)
        );
        assert_eq!(
            map_evmc_halt(EVMC_STACK_OVERFLOW),
            Some(InstructionResult::StackOverflow)
        );
        assert_eq!(
            map_evmc_halt(EVMC_STACK_UNDERFLOW),
            Some(InstructionResult::StackUnderflow)
        );
        assert_eq!(
            map_evmc_halt(EVMC_BAD_JUMP_DESTINATION),
            Some(InstructionResult::InvalidJump)
        );
        assert_eq!(
            map_evmc_halt(EVMC_INVALID_MEMORY_ACCESS),
            Some(InstructionResult::OutOfOffset)
        );
        assert_eq!(
            map_evmc_halt(EVMC_ARGUMENT_OUT_OF_RANGE),
            Some(InstructionResult::OutOfOffset)
        );
        assert_eq!(
            map_evmc_halt(EVMC_CALL_DEPTH_EXCEEDED),
            Some(InstructionResult::CallTooDeep)
        );
        assert_eq!(
            map_evmc_halt(EVMC_PRECOMPILE_FAILURE),
            Some(InstructionResult::PrecompileError)
        );
        assert_eq!(
            map_evmc_halt(EVMC_INSUFFICIENT_BALANCE),
            Some(InstructionResult::OutOfFunds)
        );
        assert_eq!(
            map_evmc_halt(EVMC_OUT_OF_GAS),
            Some(InstructionResult::OutOfGas)
        );
    }

    #[test]
    fn ambiguous_evmc_halts_remain_fail_closed() {
        assert_eq!(map_evmc_halt(EVMC_STATIC_MODE_VIOLATION), None);
        assert_eq!(map_evmc_halt(EVMC_CONTRACT_VALIDATION_FAILURE), None);
        assert_eq!(map_evmc_halt(i32::MAX), None);
    }
}
