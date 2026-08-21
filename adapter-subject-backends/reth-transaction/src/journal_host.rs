//! EVMC host backend backed by the active REVM journal.
//!
//! This bridge deliberately uses the same [`Journal`] owned by the transaction
//! shell. Database errors are converted to sticky EVMC host faults, and frame
//! checkpoints are resolved strictly in LIFO order.

use alloy_evm::{precompiles::PrecompilesMap, Database};
use alloy_primitives::{Address as RevmAddress, Bytes, Log, B256, U256};
use reth_dtvm_adapter::{
    ffi::{
        EVMC_ARGUMENT_OUT_OF_RANGE, EVMC_BAD_JUMP_DESTINATION, EVMC_CALL, EVMC_CALLCODE,
        EVMC_CALL_DEPTH_EXCEEDED, EVMC_CONTRACT_VALIDATION_FAILURE, EVMC_CREATE, EVMC_CREATE2,
        EVMC_DELEGATECALL, EVMC_FAILURE, EVMC_INSUFFICIENT_BALANCE, EVMC_INVALID_INSTRUCTION,
        EVMC_INVALID_MEMORY_ACCESS, EVMC_OUT_OF_GAS, EVMC_PRECOMPILE_FAILURE, EVMC_REVERT,
        EVMC_STACK_OVERFLOW, EVMC_STACK_UNDERFLOW, EVMC_STATIC, EVMC_STATIC_MODE_VIOLATION,
        EVMC_SUCCESS, EVMC_UNDEFINED_INSTRUCTION,
    },
    host::{
        classify_storage_status, Address, HostBackend, HostFault, LogEntry, NestedCallPreparation,
        NestedCallRequest, NestedCallResult, Word,
    },
};
use revm::{
    context::{BlockEnv, CfgEnv, Context, TxEnv},
    context_interface::{
        context::ContextError,
        journaled_state::{JournalCheckpoint, JournalLoadError},
        local::OutFrame,
        CreateScheme, JournalTr,
    },
    handler::{EthFrame, FrameResult, ItemOrResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallInput, CallInputs,
        CallScheme, CallValue, CreateInputs, FrameInput, Gas, InstructionResult, InterpreterAction,
        InterpreterResult, SharedMemory,
    },
};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Host backend for the standard REVM Ethereum context.
///
/// The concrete context shape intentionally matches
/// `alloy_evm::eth::EthEvmContext<DB>`. Keeping `DB` generic lets the bridge use
/// [`crate::StrictDb`] in the experiment while retaining the exact same journal
/// operations required by a Reth state database.
pub struct JournalHost<'a, DB: Database> {
    context: &'a mut Context<BlockEnv, TxEnv, CfgEnv, DB>,
    precompiles: &'a mut PrecompilesMap,
    checkpoints: Vec<JournalCheckpoint>,
    nested_frames: Vec<EthFrame>,
}

/// Name used by the transaction adapter while [`JournalHost`] remains the
/// reader-facing export.
pub type JournalBackend<'a, DB> = JournalHost<'a, DB>;

impl<'a, DB: Database> JournalHost<'a, DB> {
    /// Borrows the active transaction context and its journal.
    pub fn new(
        context: &'a mut Context<BlockEnv, TxEnv, CfgEnv, DB>,
        precompiles: &'a mut PrecompilesMap,
    ) -> Self {
        Self {
            context,
            precompiles,
            checkpoints: Vec::new(),
            nested_frames: Vec::new(),
        }
    }

    /// Returns the number of EVMC-owned checkpoints.
    pub fn checkpoint_depth(&self) -> usize {
        self.checkpoints.len()
    }

    fn latch_db_error(&mut self, error: DB::Error) -> HostFault {
        let message = error.to_string();
        if self.context.error.is_ok() {
            self.context.error = Err(revm::context::ContextError::Db(error));
        }
        HostFault::Backend(message)
    }

    fn ensure_account_loaded(&mut self, address: RevmAddress) -> Result<(), HostFault> {
        let result = self.context.journaled_state.load_account(address);
        match result {
            Ok(_) => Ok(()),
            Err(error) => Err(self.latch_db_error(error)),
        }
    }

    fn latch_context_error(&mut self, error: ContextError<DB::Error>) -> HostFault {
        match error {
            ContextError::Db(error) => self.latch_db_error(error),
            ContextError::Custom(message) => {
                if self.context.error.is_ok() {
                    self.context.error = Err(ContextError::Custom(message.clone()));
                }
                HostFault::Backend(message)
            }
        }
    }

    fn init_checkpoint(&self) -> (usize, JournalCheckpoint) {
        let inner = &self.context.journaled_state.inner;
        (
            inner.depth,
            JournalCheckpoint {
                log_i: inner.logs.len(),
                journal_i: inner.journal.len(),
                selfdestructed_i: inner.selfdestructed_addresses.len(),
            },
        )
    }

    fn rollback_failed_init(
        &mut self,
        initial_depth: usize,
        checkpoint: JournalCheckpoint,
    ) -> Result<(), HostFault> {
        let current_depth = self.context.journaled_state.inner.depth;
        if current_depth < initial_depth {
            return Err(HostFault::Backend(format!(
                "nested CALL initialization reduced journal depth from {initial_depth} to {current_depth}"
            )));
        }
        while self.context.journaled_state.inner.depth > initial_depth {
            self.context.journaled_state.checkpoint_revert(checkpoint);
        }
        Ok(())
    }

    fn begin_create(
        &mut self,
        request: &NestedCallRequest,
    ) -> Result<NestedCallPreparation, HostFault> {
        let depth = usize::try_from(request.depth)
            .map_err(|_| HostFault::NestedCallDepthInvalid(request.depth))?;
        let scheme = match request.kind {
            EVMC_CREATE => CreateScheme::Create,
            EVMC_CREATE2 => CreateScheme::Create2 {
                salt: to_u256(request.create2_salt),
            },
            kind => return Err(HostFault::NestedCallUnsupported { kind }),
        };
        let inputs = CreateInputs::new(
            to_revm_address(request.sender),
            scheme,
            to_u256(request.value),
            Bytes::from(request.input.clone()),
            request.gas,
            0,
        );
        let init = FrameInit {
            depth,
            memory: SharedMemory::new(),
            frame_input: FrameInput::Create(Box::new(inputs)),
        };
        let mut frame = EthFrame::<EthInterpreter>::invalid();
        let (initial_depth, init_checkpoint) = self.init_checkpoint();
        let initialized = catch_unwind(AssertUnwindSafe(|| {
            EthFrame::init_with_context(
                OutFrame::new_init(&mut frame),
                self.context,
                self.precompiles,
                init,
            )
        }));
        let initialized = match initialized {
            Ok(Ok(initialized)) => initialized,
            Ok(Err(error)) => {
                self.rollback_failed_init(initial_depth, init_checkpoint)?;
                return Err(self.latch_context_error(error));
            }
            Err(_) => {
                self.rollback_failed_init(initial_depth, init_checkpoint)?;
                return Err(HostFault::Backend(
                    "nested CREATE frame initialization panicked".into(),
                ));
            }
        };
        match initialized {
            ItemOrResult::Item(_) => {
                let expected_depth = initial_depth.checked_add(1).ok_or_else(|| {
                    HostFault::Backend("nested CREATE journal depth overflowed".into())
                })?;
                if self.context.journaled_state.inner.depth != expected_depth {
                    self.rollback_failed_init(initial_depth, init_checkpoint)?;
                    return Err(HostFault::Backend(format!(
                        "nested CREATE initialization left journal depth {}, expected {expected_depth}",
                        self.context.journaled_state.inner.depth
                    )));
                }
                let recipient = from_revm_address(frame.interpreter.input.target_address);
                self.nested_frames.push(frame);
                Ok(NestedCallPreparation::Execute {
                    code: request.input.clone(),
                    recipient,
                    input: Vec::new(),
                })
            }
            ItemOrResult::Result(result) => {
                if self.context.journaled_state.inner.depth != initial_depth {
                    self.rollback_failed_init(initial_depth, init_checkpoint)?;
                    return Err(HostFault::Backend(format!(
                        "nested CREATE immediate result left journal depth {}, expected {initial_depth}",
                        self.context.journaled_state.inner.depth
                    )));
                }
                Ok(NestedCallPreparation::Immediate(frame_result_to_evmc(
                    result,
                )?))
            }
        }
    }
}

impl<DB: Database> HostBackend for JournalHost<'_, DB> {
    fn checkpoint(&mut self) -> Result<(), HostFault> {
        let checkpoint = self.context.journaled_state.checkpoint();
        self.checkpoints.push(checkpoint);
        Ok(())
    }

    fn commit_checkpoint(&mut self) -> Result<(), HostFault> {
        if self.checkpoints.is_empty() {
            return Err(HostFault::Backend(
                "attempted to commit an EVMC checkpoint with an empty stack".to_string(),
            ));
        }

        self.context.journaled_state.checkpoint_commit();
        self.checkpoints.pop();
        Ok(())
    }

    fn revert_checkpoint(&mut self) -> Result<(), HostFault> {
        let checkpoint = self.checkpoints.last().copied().ok_or_else(|| {
            HostFault::Backend(
                "attempted to revert an EVMC checkpoint with an empty stack".to_string(),
            )
        })?;

        self.context.journaled_state.checkpoint_revert(checkpoint);
        self.checkpoints.pop();
        Ok(())
    }

    fn account_exists(&mut self, address: Address) -> Result<bool, HostFault> {
        let spec = self.context.cfg.spec;
        let result = self
            .context
            .journaled_state
            .load_account(to_revm_address(address));
        match result {
            Ok(load) => Ok(!load.state_clear_aware_is_empty(spec)),
            Err(error) => Err(self.latch_db_error(error)),
        }
    }

    fn get_storage(&mut self, address: Address, key: Word) -> Result<Word, HostFault> {
        let address = to_revm_address(address);
        self.ensure_account_loaded(address)?;
        let result = self.context.journaled_state.sload(address, to_u256(key));
        match result {
            Ok(load) => Ok(from_u256(load.data)),
            Err(error) => Err(self.latch_db_error(error)),
        }
    }

    fn set_storage(&mut self, address: Address, key: Word, value: Word) -> Result<i32, HostFault> {
        let address = to_revm_address(address);
        self.ensure_account_loaded(address)?;
        let result = self
            .context
            .journaled_state
            .sstore(address, to_u256(key), to_u256(value));
        let load = match result {
            Ok(load) => load,
            Err(error) => return Err(self.latch_db_error(error)),
        };
        let values = load.data;
        Ok(classify_storage_status(
            from_u256(values.original_value),
            from_u256(values.present_value),
            from_u256(values.new_value),
        ))
    }

    fn get_balance(&mut self, address: Address) -> Result<Word, HostFault> {
        let result = self
            .context
            .journaled_state
            .load_account(to_revm_address(address));
        match result {
            Ok(load) => Ok(from_u256(load.info.balance)),
            Err(error) => Err(self.latch_db_error(error)),
        }
    }

    fn get_code(&mut self, address: Address) -> Result<Vec<u8>, HostFault> {
        let result = self
            .context
            .journaled_state
            .load_account_with_code(to_revm_address(address));
        match result {
            Ok(load) => Ok(load
                .info
                .code
                .as_ref()
                .map(|code| code.original_bytes().to_vec())
                .unwrap_or_default()),
            Err(error) => Err(self.latch_db_error(error)),
        }
    }

    fn get_code_hash(&mut self, address: Address) -> Result<Word, HostFault> {
        let result = self
            .context
            .journaled_state
            .code_hash(to_revm_address(address));
        match result {
            Ok(load) => Ok(from_b256(load.data)),
            Err(error) => Err(self.latch_db_error(error)),
        }
    }

    fn selfdestruct(&mut self, address: Address, beneficiary: Address) -> Result<bool, HostFault> {
        match self.context.journaled_state.selfdestruct(
            to_revm_address(address),
            to_revm_address(beneficiary),
            false,
        ) {
            Ok(load) => Ok(!load.data.previously_destroyed),
            Err(JournalLoadError::DBError(error)) => Err(self.latch_db_error(error)),
            Err(JournalLoadError::ColdLoadSkipped) => Err(HostFault::Backend(
                "SELFDESTRUCT unexpectedly skipped a cold beneficiary load".into(),
            )),
        }
    }

    fn block_hash(&mut self, number: i64) -> Result<Word, HostFault> {
        let number = u64::try_from(number).map_err(|_| HostFault::MissingBlockHash(number))?;
        let result = self.context.journaled_state.database.block_hash(number);
        match result {
            Ok(hash) => Ok(from_b256(hash)),
            Err(error) => Err(self.latch_db_error(error)),
        }
    }

    fn emit_log(&mut self, log: LogEntry) -> Result<(), HostFault> {
        let topics = log.topics.into_iter().map(to_b256).collect();
        self.context.journaled_state.log(Log::new_unchecked(
            to_revm_address(log.address),
            topics,
            Bytes::from(log.data),
        ));
        Ok(())
    }

    fn access_account(&mut self, address: Address) -> Result<bool, HostFault> {
        let result = self
            .context
            .journaled_state
            .load_account_mut_skip_cold_load(to_revm_address(address), true);
        match result {
            Ok(load) => Ok(!load.is_cold),
            Err(JournalLoadError::ColdLoadSkipped) => Ok(false),
            Err(JournalLoadError::DBError(error)) => Err(self.latch_db_error(error)),
        }
    }

    fn is_account_warm(&mut self, address: Address) -> Result<bool, HostFault> {
        let address = to_revm_address(address);
        let journal = &self.context.journaled_state.inner;

        if journal.warm_addresses.is_warm(&address) {
            return Ok(true);
        }

        Ok(journal
            .state
            .get(&address)
            .is_some_and(|account| !account.is_cold_transaction_id(journal.transaction_id)))
    }

    fn access_storage(&mut self, address: Address, key: Word) -> Result<bool, HostFault> {
        let address = to_revm_address(address);
        let result = self
            .context
            .journaled_state
            .sload_skip_cold_load(address, to_u256(key), true);
        match result {
            Ok(load) => Ok(!load.is_cold),
            Err(JournalLoadError::ColdLoadSkipped) => Ok(false),
            Err(JournalLoadError::DBError(error)) => Err(self.latch_db_error(error)),
        }
    }

    fn get_transient_storage(&mut self, address: Address, key: Word) -> Result<Word, HostFault> {
        let address = to_revm_address(address);
        self.ensure_account_loaded(address)?;
        Ok(from_u256(
            self.context.journaled_state.tload(address, to_u256(key)),
        ))
    }

    fn set_transient_storage(
        &mut self,
        address: Address,
        key: Word,
        value: Word,
    ) -> Result<(), HostFault> {
        let address = to_revm_address(address);
        self.ensure_account_loaded(address)?;
        self.context
            .journaled_state
            .tstore(address, to_u256(key), to_u256(value));
        Ok(())
    }

    fn begin_call(
        &mut self,
        request: &NestedCallRequest,
    ) -> Result<NestedCallPreparation, HostFault> {
        if matches!(request.kind, EVMC_CREATE | EVMC_CREATE2) {
            return self.begin_create(request);
        }
        let target = to_revm_address(request.recipient);
        let code_address = to_revm_address(request.code_address);
        let loaded = match self
            .context
            .journaled_state
            .load_account_with_code(code_address)
        {
            Ok(loaded) => loaded,
            Err(error) => return Err(self.latch_db_error(error)),
        };
        let code_hash = loaded.info.code_hash();
        let code = loaded.info.code.clone().unwrap_or_default();
        let code_bytes = code.original_bytes().to_vec();
        let depth = usize::try_from(request.depth)
            .map_err(|_| HostFault::NestedCallDepthInvalid(request.depth))?;
        let is_static = request.flags & EVMC_STATIC != 0;
        let (scheme, value) = match request.kind {
            EVMC_CALL if is_static => (
                CallScheme::StaticCall,
                CallValue::Transfer(to_u256(request.value)),
            ),
            EVMC_CALL => (
                CallScheme::Call,
                CallValue::Transfer(to_u256(request.value)),
            ),
            EVMC_DELEGATECALL => (
                CallScheme::DelegateCall,
                CallValue::Apparent(to_u256(request.value)),
            ),
            EVMC_CALLCODE => (
                CallScheme::CallCode,
                CallValue::Transfer(to_u256(request.value)),
            ),
            kind => return Err(HostFault::NestedCallUnsupported { kind }),
        };
        let inputs = CallInputs {
            input: CallInput::Bytes(Bytes::from(request.input.clone())),
            return_memory_offset: 0..0,
            gas_limit: request.gas,
            reservoir: 0,
            bytecode_address: code_address,
            known_bytecode: (code_hash, code),
            target_address: target,
            caller: to_revm_address(request.sender),
            value,
            scheme,
            is_static,
            charged_new_account_state_gas: false,
        };
        let init = FrameInit {
            depth,
            memory: SharedMemory::new(),
            frame_input: FrameInput::Call(Box::new(inputs)),
        };
        let mut frame = EthFrame::<EthInterpreter>::invalid();
        let (initial_depth, init_checkpoint) = self.init_checkpoint();
        let initialized = catch_unwind(AssertUnwindSafe(|| {
            EthFrame::init_with_context(
                OutFrame::new_init(&mut frame),
                self.context,
                self.precompiles,
                init,
            )
        }));
        let initialized = match initialized {
            Ok(Ok(initialized)) => initialized,
            Ok(Err(error)) => {
                self.rollback_failed_init(initial_depth, init_checkpoint)?;
                return Err(self.latch_context_error(error));
            }
            Err(_) => {
                self.rollback_failed_init(initial_depth, init_checkpoint)?;
                return Err(HostFault::Backend(
                    "nested call frame initialization panicked".into(),
                ));
            }
        };
        match initialized {
            ItemOrResult::Item(_) => {
                let expected_depth = initial_depth.checked_add(1).ok_or_else(|| {
                    HostFault::Backend("nested CALL journal depth overflowed".into())
                })?;
                if self.context.journaled_state.inner.depth != expected_depth {
                    self.rollback_failed_init(initial_depth, init_checkpoint)?;
                    return Err(HostFault::Backend(format!(
                        "nested CALL initialization left journal depth {}, expected {expected_depth}",
                        self.context.journaled_state.inner.depth
                    )));
                }
                self.nested_frames.push(frame);
                Ok(NestedCallPreparation::Execute {
                    code: code_bytes,
                    recipient: request.recipient,
                    input: request.input.clone(),
                })
            }
            ItemOrResult::Result(result) => {
                if self.context.journaled_state.inner.depth != initial_depth {
                    self.rollback_failed_init(initial_depth, init_checkpoint)?;
                    return Err(HostFault::Backend(format!(
                        "nested CALL immediate result left journal depth {}, expected {initial_depth}",
                        self.context.journaled_state.inner.depth
                    )));
                }
                Ok(NestedCallPreparation::Immediate(frame_result_to_evmc(
                    result,
                )?))
            }
        }
    }

    fn finish_call(&mut self, result: NestedCallResult) -> Result<NestedCallResult, HostFault> {
        let mut frame = self.nested_frames.pop().ok_or_else(|| {
            HostFault::Backend("nested CALL finish requested with no initialized frame".into())
        })?;
        if frame.interpreter.gas.reservoir() != 0 {
            self.context
                .journaled_state
                .checkpoint_revert(frame.checkpoint);
            return Err(HostFault::Backend(
                "nested EIP-8037 reservoir is unsupported".into(),
            ));
        }
        if result.gas_left > frame.interpreter.gas.limit() {
            self.context
                .journaled_state
                .checkpoint_revert(frame.checkpoint);
            return Err(HostFault::Backend(
                "nested result retained more gas than the Reth child frame".into(),
            ));
        }
        let forced_failure = result.status_code == EVMC_FAILURE;
        let instruction = match evmc_to_instruction(result.status_code) {
            Ok(instruction) => instruction,
            Err(error) => {
                self.context
                    .journaled_state
                    .checkpoint_revert(frame.checkpoint);
                return Err(error);
            }
        };
        let mut gas = Gas::new(frame.interpreter.gas.limit());
        gas.set_remaining(result.gas_left);
        gas.set_refund(result.gas_refund);
        let action = InterpreterAction::Return(InterpreterResult {
            result: instruction,
            gas,
            output: Bytes::from(result.output),
        });
        let closed =
            match frame.process_next_action::<_, ContextError<DB::Error>>(self.context, action) {
                Ok(closed) => closed,
                Err(error) => {
                    // `Return` has no fallible path before checkpoint resolution
                    // today. Keep the error arm fail-closed if that API changes.
                    self.context
                        .journaled_state
                        .checkpoint_revert(frame.checkpoint);
                    return Err(self.latch_context_error(error));
                }
            };
        let ItemOrResult::Result(result) = closed else {
            return Err(HostFault::Backend(
                "nested CALL return unexpectedly requested another frame".into(),
            ));
        };
        if forced_failure {
            return Ok(NestedCallResult {
                status_code: EVMC_FAILURE,
                gas_left: 0,
                gas_refund: 0,
                output: Vec::new(),
                create_address: None,
            });
        }
        frame_result_to_evmc(result)
    }
}

fn frame_result_to_evmc(result: FrameResult) -> Result<NestedCallResult, HostFault> {
    let (result, create_address, is_create) = match result {
        FrameResult::Call(outcome) => (outcome.result, None, false),
        FrameResult::Create(outcome) => {
            (outcome.result, outcome.address.map(from_revm_address), true)
        }
    };
    let status_code = instruction_to_evmc(result.result)?;
    let returns_gas = result.result.is_ok_or_revert();
    Ok(NestedCallResult {
        status_code,
        gas_left: returns_gas
            .then(|| result.gas.remaining())
            .unwrap_or_default(),
        gas_refund: result
            .result
            .is_ok()
            .then(|| result.gas.refunded())
            .unwrap_or_default(),
        output: (!is_create || result.result == InstructionResult::Revert)
            .then(|| result.output.to_vec())
            .unwrap_or_default(),
        create_address,
    })
}

fn instruction_to_evmc(result: InstructionResult) -> Result<i32, HostFault> {
    Ok(match result {
        InstructionResult::Stop | InstructionResult::Return | InstructionResult::SelfDestruct => {
            EVMC_SUCCESS
        }
        InstructionResult::Revert => EVMC_REVERT,
        InstructionResult::CallTooDeep => EVMC_CALL_DEPTH_EXCEEDED,
        InstructionResult::OutOfFunds => EVMC_INSUFFICIENT_BALANCE,
        InstructionResult::OutOfGas
        | InstructionResult::MemoryOOG
        | InstructionResult::MemoryLimitOOG
        | InstructionResult::PrecompileOOG
        | InstructionResult::InvalidOperandOOG
        | InstructionResult::ReentrancySentryOOG => EVMC_OUT_OF_GAS,
        InstructionResult::InvalidFEOpcode => EVMC_INVALID_INSTRUCTION,
        InstructionResult::OpcodeNotFound => EVMC_UNDEFINED_INSTRUCTION,
        InstructionResult::StackOverflow => EVMC_STACK_OVERFLOW,
        InstructionResult::StackUnderflow => EVMC_STACK_UNDERFLOW,
        InstructionResult::InvalidJump => EVMC_BAD_JUMP_DESTINATION,
        InstructionResult::OutOfOffset => EVMC_INVALID_MEMORY_ACCESS,
        InstructionResult::PrecompileError => EVMC_PRECOMPILE_FAILURE,
        InstructionResult::CallNotAllowedInsideStatic
        | InstructionResult::StateChangeDuringStaticCall => EVMC_STATIC_MODE_VIOLATION,
        InstructionResult::InvalidEOFInitCode
        | InstructionResult::CreateInitCodeStartingEF00
        | InstructionResult::CreateContractSizeLimit
        | InstructionResult::CreateContractStartingWithEF
        | InstructionResult::CreateInitCodeSizeLimit => EVMC_CONTRACT_VALIDATION_FAILURE,
        InstructionResult::CreateCollision
        | InstructionResult::OverflowPayment
        | InstructionResult::NonceOverflow => EVMC_FAILURE,
        InstructionResult::FatalExternalError => {
            return Err(HostFault::Backend(
                "REVM FatalExternalError is a backend/internal fault".into(),
            ));
        }
        other => {
            return Err(HostFault::Backend(format!(
                "REVM child result has no exact EVMC mapping: {other:?}"
            )));
        }
    })
}

fn evmc_to_instruction(status: i32) -> Result<InstructionResult, HostFault> {
    Ok(match status {
        EVMC_SUCCESS => InstructionResult::Return,
        EVMC_REVERT => InstructionResult::Revert,
        EVMC_FAILURE => InstructionResult::FatalExternalError,
        EVMC_OUT_OF_GAS => InstructionResult::OutOfGas,
        EVMC_INVALID_INSTRUCTION => InstructionResult::InvalidFEOpcode,
        EVMC_UNDEFINED_INSTRUCTION => InstructionResult::OpcodeNotFound,
        EVMC_STACK_OVERFLOW => InstructionResult::StackOverflow,
        EVMC_STACK_UNDERFLOW => InstructionResult::StackUnderflow,
        EVMC_BAD_JUMP_DESTINATION => InstructionResult::InvalidJump,
        EVMC_INVALID_MEMORY_ACCESS | EVMC_ARGUMENT_OUT_OF_RANGE => InstructionResult::OutOfOffset,
        EVMC_CALL_DEPTH_EXCEEDED => InstructionResult::CallTooDeep,
        EVMC_PRECOMPILE_FAILURE => InstructionResult::PrecompileError,
        EVMC_INSUFFICIENT_BALANCE => InstructionResult::OutOfFunds,
        EVMC_STATIC_MODE_VIOLATION => InstructionResult::StateChangeDuringStaticCall,
        EVMC_CONTRACT_VALIDATION_FAILURE => InstructionResult::CreateContractStartingWithEF,
        other => {
            return Err(HostFault::Backend(format!(
                "nested EVMC result is not exactly representable in REVM: {other}"
            )));
        }
    })
}

fn to_revm_address(address: Address) -> RevmAddress {
    RevmAddress::from(address.0)
}

fn from_revm_address(address: RevmAddress) -> Address {
    Address(address.0 .0)
}

fn to_u256(value: Word) -> U256 {
    U256::from_be_bytes(value.0)
}

fn from_u256(value: U256) -> Word {
    Word(value.to_be_bytes::<32>())
}

fn to_b256(value: Word) -> B256 {
    B256::from(value.0)
}

fn from_b256(value: B256) -> Word {
    Word(value.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StrictDb;
    use alloy_evm::precompiles::DynPrecompile;
    use alloy_primitives::keccak256;
    use revm::{
        handler::EthPrecompiles,
        precompile::{PrecompileError, PrecompileId},
        primitives::hardfork::SpecId,
        state::AccountInfo,
    };

    const CALLER: RevmAddress = RevmAddress::new([0x11; 20]);
    const TARGET: RevmAddress = RevmAddress::new([0x22; 20]);

    #[test]
    fn access_callbacks_defer_cold_account_and_storage_loads() {
        let first_key = U256::from(7);
        let second_key = U256::from(8);
        let mut db = StrictDb::default();
        db.insert_account(
            TARGET,
            AccountInfo {
                balance: U256::from(11),
                ..Default::default()
            },
        )
        .unwrap();
        db.cover_storage(TARGET, first_key, U256::from(13)).unwrap();
        db.cover_storage(TARGET, second_key, U256::from(17))
            .unwrap();
        let mut context = Context::<BlockEnv, TxEnv, CfgEnv, StrictDb>::new(db, SpecId::OSAKA);
        let mut precompiles = PrecompilesMap::from(EthPrecompiles::new(SpecId::OSAKA));
        let mut host = JournalHost::new(&mut context, &mut precompiles);
        let target = from_revm_address(TARGET);

        assert!(!host.access_account(target).unwrap());
        assert!(host.context.journaled_state.database.accesses().is_empty());
        assert!(host.account_exists(target).unwrap());
        assert_eq!(host.get_balance(target).unwrap(), from_u256(U256::from(11)));
        assert_eq!(
            host.context.journaled_state.database.accesses(),
            &[crate::DbAccess::Basic(TARGET)]
        );
        assert!(host.access_account(target).unwrap());

        assert!(!host.access_storage(target, from_u256(first_key)).unwrap());
        assert_eq!(host.context.journaled_state.database.accesses().len(), 1);
        assert_eq!(
            host.get_storage(target, from_u256(first_key)).unwrap(),
            from_u256(U256::from(13))
        );
        assert!(host.access_storage(target, from_u256(first_key)).unwrap());

        assert!(!host.access_storage(target, from_u256(second_key)).unwrap());
        assert_eq!(
            host.set_storage(target, from_u256(second_key), from_u256(U256::from(19)))
                .unwrap(),
            reth_dtvm_adapter::host::EVMC_STORAGE_MODIFIED
        );
        assert!(host.access_storage(target, from_u256(second_key)).unwrap());
        assert_eq!(
            host.context.journaled_state.database.accesses(),
            &[
                crate::DbAccess::Basic(TARGET),
                crate::DbAccess::Storage(TARGET, first_key),
                crate::DbAccess::Storage(TARGET, second_key),
            ]
        );
    }

    fn call_request() -> NestedCallRequest {
        NestedCallRequest {
            kind: reth_dtvm_adapter::ffi::EVMC_CALL,
            flags: 0,
            depth: 1,
            gas: 100_000,
            recipient: Address([0x22; 20]),
            sender: Address([0x11; 20]),
            input: Vec::new(),
            value: Word::ZERO,
            create2_salt: Word::ZERO,
            code_address: Address([0x22; 20]),
        }
    }

    fn create2_request(depth: i32) -> NestedCallRequest {
        NestedCallRequest {
            kind: EVMC_CREATE2,
            flags: 0,
            depth,
            gas: 100_000,
            recipient: Address::default(),
            sender: Address([0x11; 20]),
            input: vec![0x00],
            value: Word::ZERO,
            create2_salt: Word::from_u64(7),
            code_address: Address::default(),
        }
    }

    #[test]
    fn create2_frame_uses_reth_address_empty_calldata_and_code_deposit() {
        let request = create2_request(1);
        let created = CALLER.create2(U256::from(7).to_be_bytes(), keccak256(&request.input));
        let mut db = StrictDb::default();
        db.insert_account(
            CALLER,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        )
        .unwrap();
        db.cover_absent_account(created);
        let mut context = Context::<BlockEnv, TxEnv, CfgEnv, StrictDb>::new(db, SpecId::OSAKA);
        let mut precompiles = PrecompilesMap::from(EthPrecompiles::new(SpecId::OSAKA));
        let mut host = JournalHost::new(&mut context, &mut precompiles);

        let preparation = host.begin_call(&request).unwrap();
        assert_eq!(
            preparation,
            NestedCallPreparation::Execute {
                code: request.input.clone(),
                recipient: from_revm_address(created),
                input: Vec::new(),
            }
        );
        assert_eq!(host.nested_frames.len(), 1);
        let result = host
            .finish_call(NestedCallResult {
                status_code: EVMC_SUCCESS,
                gas_left: 50_000,
                gas_refund: 0,
                output: vec![0x00],
                create_address: None,
            })
            .unwrap();
        assert_eq!(result.status_code, EVMC_SUCCESS);
        assert_eq!(result.create_address, Some(from_revm_address(created)));
        assert!(result.output.is_empty());
        assert!(host.nested_frames.is_empty());
        let created = from_revm_address(created);
        assert!(host.selfdestruct(created, created).unwrap());
        assert!(!host.selfdestruct(created, created).unwrap());
    }

    #[test]
    fn create2_depth_failure_is_immediate_and_returns_unused_gas() {
        let mut context =
            Context::<BlockEnv, TxEnv, CfgEnv, StrictDb>::new(StrictDb::default(), SpecId::OSAKA);
        let mut precompiles = PrecompilesMap::from(EthPrecompiles::new(SpecId::OSAKA));
        let mut host = JournalHost::new(&mut context, &mut precompiles);

        let result = host.begin_call(&create2_request(1_025)).unwrap();
        let NestedCallPreparation::Immediate(result) = result else {
            panic!("depth failure must not initialize a CREATE2 frame");
        };
        assert_eq!(result.status_code, EVMC_CALL_DEPTH_EXCEEDED);
        assert_eq!(result.gas_left, 100_000);
        assert_eq!(result.create_address, None);
        assert!(host.nested_frames.is_empty());
        assert_eq!(host.context.journaled_state.inner.depth, 0);
    }

    fn assert_failing_precompile_restores_init_checkpoint(should_panic: bool) {
        let mut db = StrictDb::default();
        db.insert_account(
            CALLER,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_account(
            TARGET,
            AccountInfo {
                balance: U256::from(7),
                ..Default::default()
            },
        )
        .unwrap();
        let mut context = Context::<BlockEnv, TxEnv, CfgEnv, StrictDb>::new(db, SpecId::OSAKA);
        context
            .journaled_state
            .load_account_with_code(TARGET)
            .unwrap();
        context.journaled_state.load_account(CALLER).unwrap();
        let initial_depth = context.journaled_state.inner.depth;
        let initial_journal_len = context.journaled_state.inner.journal.len();
        let initial_logs = context.journaled_state.inner.logs.clone();
        let initial_state = context.journaled_state.inner.state.clone();

        let mut precompiles = PrecompilesMap::from(EthPrecompiles::new(SpecId::OSAKA));
        precompiles.apply_precompile(&TARGET, |_| {
            Some(DynPrecompile::new(
                PrecompileId::custom(if should_panic {
                    "checkpoint-panic"
                } else {
                    "checkpoint-error"
                }),
                move |mut input| {
                    input
                        .internals_mut()
                        .set_balance(TARGET, U256::from(999))
                        .unwrap();
                    input.internals_mut().log(Log::new_unchecked(
                        TARGET,
                        vec![B256::repeat_byte(0x55)],
                        Bytes::from_static(b"must-revert"),
                    ));
                    if should_panic {
                        panic!("dynamic precompile panic after journal mutation");
                    }
                    Err(PrecompileError::Fatal(
                        "dynamic precompile error after journal mutation".into(),
                    ))
                },
            ))
        });

        let mut host = JournalHost::new(&mut context, &mut precompiles);
        let error = host
            .begin_call(&call_request())
            .expect_err("precompile must abort child initialization");
        if should_panic {
            assert!(matches!(
                error,
                HostFault::Backend(message)
                    if message.contains("initialization panicked")
            ));
        } else {
            assert!(matches!(
                error,
                HostFault::Backend(message)
                    if message.contains("dynamic precompile error")
            ));
        }
        assert!(host.nested_frames.is_empty());
        drop(host);

        assert_eq!(context.journaled_state.inner.depth, initial_depth);
        assert_eq!(
            context.journaled_state.inner.journal.len(),
            initial_journal_len
        );
        assert_eq!(context.journaled_state.inner.logs, initial_logs);
        assert_eq!(context.journaled_state.inner.state, initial_state);
    }

    #[test]
    fn dynamic_precompile_error_reverts_unowned_init_checkpoint() {
        assert_failing_precompile_restores_init_checkpoint(false);
    }

    #[test]
    fn dynamic_precompile_panic_reverts_unowned_init_checkpoint() {
        assert_failing_precompile_restores_init_checkpoint(true);
    }
}
