use alloy_evm::{
    eth::{EthEvmContext, EthEvmFactory},
    Evm, EvmEnv, EvmFactory,
};
use alloy_primitives::{address, bytes, keccak256, Address, Bytes, TxKind, B256, U256};
use reth_chainspec::MAINNET;
use reth_dtvm_adapter::{AccessEvent, Address as DtvmAddress, Word};
use reth_dtvm_transaction_adapter::{
    DbAccess, DtvmEvmFactory, StrictDb, StrictDbError, STORAGE_LOG_RETURN, SUPPORTED_TX_GAS_LIMIT,
};
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::EthEvmConfig;
use revm::{
    bytecode::{opcode, Bytecode},
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{
        result::{EVMError, ExecutionResult, InvalidTransaction, SuccessReason},
        transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    },
    handler::{EvmTr, ExecuteEvm, SYSTEM_ADDRESS},
    inspector::Inspector,
    interpreter::{interpreter::EthInterpreter, interpreter_types::Jumps, Interpreter},
    primitives::hardfork::SpecId,
    state::{Account, AccountId, AccountInfo, EvmState},
};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::PathBuf};

const SENDER: Address = Address::new([0x22; 20]);
const RECIPIENT: Address = Address::new([0x11; 20]);
const BENEFICIARY: Address = Address::new([0x33; 20]);
const CHILD: Address = Address::new([0x44; 20]);
const GRANDCHILD: Address = Address::new([0x55; 20]);
const EMPTY_CHILD: Address = Address::new([0x66; 20]);
const HISTORY_STORAGE_ADDRESS: Address = address!("0x0000F90827F1C53a10cb7A02335B175320002935");
const LOW_REGULAR_CONTRACT: Address = address!("0x0000000000000000000000000000000000000101");
static HISTORY_STORAGE_RETURN_CODE: Bytes = bytes!(
    "0x3373fffffffffffffffffffffffffffffffffffffffe14604657602036036042575f35600143038111604257611fff81430311604257611fff9006545f5260205ff35b5f5ffd5b5f35611fff6001430306555f5ff3"
);
const GASPRICE_CALLDATA_RETURN: &[u8] = &[
    opcode::GASPRICE,
    opcode::PUSH0,
    opcode::MSTORE,
    opcode::PUSH0,
    opcode::CALLDATALOAD,
    opcode::PUSH1,
    0x20,
    opcode::MSTORE,
    opcode::PUSH1,
    0x40,
    opcode::PUSH0,
    opcode::RETURN,
];
const STORAGE_REVERT: &[u8] = &[
    opcode::PUSH1,
    0x2a,
    opcode::PUSH0,
    opcode::SSTORE,
    opcode::PUSH0,
    opcode::PUSH0,
    opcode::REVERT,
];
const STORAGE_HALT: &[u8] = &[
    opcode::PUSH1,
    0x2a,
    opcode::PUSH0,
    opcode::SSTORE,
    opcode::INVALID,
];
const STORAGE_WRITE_STOP: &[u8] = &[
    opcode::PUSH1,
    0x2a,
    opcode::PUSH0,
    opcode::SSTORE,
    opcode::STOP,
];
const STORAGE_WRITE_RETURN: &[u8] = &[
    opcode::PUSH1,
    0x2a,
    opcode::PUSH0,
    opcode::SSTORE,
    opcode::PUSH0,
    opcode::PUSH0,
    opcode::RETURN,
];
const STORAGE_READ_RETURN: &[u8] = &[
    opcode::PUSH0,
    opcode::SLOAD,
    opcode::PUSH0,
    opcode::MSTORE,
    opcode::PUSH1,
    0x20,
    opcode::PUSH0,
    opcode::RETURN,
];
const CONTEXT_STORE: &[u8] = &[
    opcode::ADDRESS,
    opcode::PUSH0,
    opcode::SSTORE,
    opcode::CALLER,
    opcode::PUSH1,
    0x01,
    opcode::SSTORE,
    opcode::CALLVALUE,
    opcode::PUSH1,
    0x02,
    opcode::SSTORE,
    opcode::PUSH0,
    opcode::PUSH0,
    opcode::RETURN,
];
const LOG_STOP: &[u8] = &[opcode::PUSH0, opcode::PUSH0, opcode::LOG0, opcode::STOP];
const STORAGE_REVERT_WITH_OUTPUT: &[u8] = &[
    opcode::PUSH1,
    0x2a,
    opcode::PUSH0,
    opcode::SSTORE,
    opcode::PUSH1,
    0x2a,
    opcode::PUSH0,
    opcode::MSTORE,
    opcode::PUSH1,
    0x20,
    opcode::PUSH0,
    opcode::REVERT,
];
const EMPTY_RETURN: &[u8] = &[opcode::PUSH0, opcode::PUSH0, opcode::RETURN];
const EMPTY_REVERT: &[u8] = &[opcode::PUSH0, opcode::PUSH0, opcode::REVERT];
const BLOBHASH_RETURN: &[u8] = &[
    opcode::PUSH0,
    opcode::BLOBHASH,
    opcode::PUSH0,
    opcode::MSTORE,
    opcode::PUSH1,
    0x20,
    opcode::PUSH0,
    opcode::RETURN,
];
const LOW_ADDRESS_RETURN: &[u8] = &[
    opcode::PUSH1,
    0x2a,
    opcode::PUSH0,
    opcode::MSTORE,
    opcode::PUSH1,
    0x20,
    opcode::PUSH0,
    opcode::RETURN,
];

#[derive(Clone, Debug, PartialEq, Eq)]
enum LogicalAttempt {
    StorageWrite(Address, U256),
    Log(Address),
    StorageRead(Address, U256),
}

#[derive(Clone, Debug, Default)]
struct AttemptInspector {
    attempts: Vec<LogicalAttempt>,
}

impl Inspector<EthEvmContext<StrictDb>> for AttemptInspector {
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut EthEvmContext<StrictDb>,
    ) {
        let address = interp.input.target_address;
        match interp.bytecode.opcode() {
            opcode::SSTORE => {
                self.attempts.push(LogicalAttempt::StorageWrite(
                    address,
                    interp.stack.peek(0).expect("allowlisted SSTORE key"),
                ));
            }
            opcode::SLOAD => {
                self.attempts.push(LogicalAttempt::StorageRead(
                    address,
                    interp.stack.peek(0).expect("allowlisted SLOAD key"),
                ));
            }
            _ => {}
        }
    }

    fn log(&mut self, _context: &mut EthEvmContext<StrictDb>, log: alloy_primitives::Log) {
        self.attempts.push(LogicalAttempt::Log(log.address));
    }
}

#[test]
fn pinned_reth_accepts_real_dtvm_factory_shape() {
    fn assert_configure_evm<T: ConfigureEvm>(_config: &T) {}

    let factory = DtvmEvmFactory::new("/provenance-only/not-loaded.so");
    let config = EthEvmConfig::new_with_evm_factory(MAINNET.clone(), factory);
    assert_configure_evm(&config);
    let _ = config.block_executor_factory();
}

#[test]
fn cloned_factory_reuses_one_thread_local_vm_but_keeps_fresh_databases() {
    let factory = DtvmEvmFactory::new("/definitely/missing/shared-subject.so");
    let first = factory.clone().create_evm(StrictDb::default(), osaka_env());
    let second = factory.create_evm(StrictDb::default(), osaka_env());

    assert_eq!(factory.vm_create_count(), 1);
    assert!(!std::ptr::eq(first.db(), second.db()));
}

#[test]
fn ordinary_osaka_transaction_matches_default_revm_and_audits_accesses() {
    let library = verified_dtvm_library();
    let fixture = strict_fixture(true, STORAGE_LOG_RETURN);
    let env = osaka_env();
    let tx = supported_tx();

    let mut reference = EthEvmFactory::default().create_evm_with_inspector(
        fixture.clone(),
        env.clone(),
        AttemptInspector::default(),
    );
    let reference_outcome = reference
        .transact_raw(tx.clone())
        .expect("default REVM reference transaction");
    let reference_attempts = reference.inspector().attempts.clone();
    let reference_db_accesses = reference.db().accesses().to_vec();

    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm
        .transact_raw(tx)
        .expect("real DTVM-backed Reth transaction");
    let dtvm_audit = dtvm.last_audit().to_vec();
    let dtvm_db_accesses = dtvm.db().accesses().to_vec();

    assert_eq!(
        dtvm_outcome.result, reference_outcome.result,
        "status/reason, output, logs, and every ResultGas field must match"
    );
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(
        dtvm_db_accesses, reference_db_accesses,
        "fresh-journal witness fetch order must match"
    );
    assert!(matches!(
        reference_db_accesses.as_slice(),
        [
            DbAccess::Basic(SENDER),
            DbAccess::Basic(RECIPIENT),
            DbAccess::Code(_),
            DbAccess::StorageByAccountId(RECIPIENT, _, key),
            DbAccess::Basic(BENEFICIARY)
        ] if key == &U256::ZERO
    ));
    assert_eq!(
        dtvm_audit,
        expected_dtvm_audit(),
        "the exact allowlisted program must make no extra EVMC host callback"
    );

    let dtvm_attempts = logical_dtvm_attempts(&dtvm_audit);
    assert_eq!(dtvm_attempts, reference_attempts);
    assert_eq!(
        reference_attempts,
        [
            LogicalAttempt::StorageWrite(RECIPIENT, U256::ZERO),
            LogicalAttempt::Log(RECIPIENT),
            LogicalAttempt::StorageRead(RECIPIENT, U256::ZERO),
        ]
    );
    assert_eq!(
        dtvm_audit
            .iter()
            .filter(|event| matches!(event, AccessEvent::StorageWarm(..)))
            .count(),
        2,
        "both SSTORE and SLOAD logical attempts must reach EVMC access_storage"
    );

    let gas = reference_outcome.result.gas();
    let output = reference_outcome
        .result
        .output()
        .expect("successful call output");
    let log = &reference_outcome.result.logs()[0];
    let slot0 = reference_outcome.state[&RECIPIENT].storage[&U256::ZERO].present_value;
    println!(
        concat!(
            "RETH_DTVM_TX_DIFF_JSON={{",
            "\"result_equal\":true,",
            "\"state_semantics_equal\":true,",
            "\"db_access_order_equal\":true,",
            "\"exact_dtvm_host_audit_equal\":true,",
            "\"logical_attempts_equal\":true,",
            "\"output\":\"{}\",",
            "\"total_gas_spent\":{},",
            "\"inner_refunded\":{},",
            "\"final_refunded\":{},",
            "\"floor_gas\":{},",
            "\"state_gas_spent_final\":{},",
            "\"tx_gas_used\":{},",
            "\"block_regular_gas_used\":{},",
            "\"block_state_gas_used\":{},",
            "\"log_count\":{},",
            "\"log_data\":\"{}\",",
            "\"log_topic0\":\"{}\",",
            "\"slot0\":\"{}\",",
            "\"db_access_count\":{},",
            "\"dtvm_audit_count\":{},",
            "\"logical_attempt_count\":{}",
            "}}"
        ),
        encode_hex(output),
        gas.total_gas_spent(),
        gas.inner_refunded(),
        gas.final_refunded(),
        gas.floor_gas(),
        gas.state_gas_spent_final(),
        gas.tx_gas_used(),
        gas.block_regular_gas_used(),
        gas.block_state_gas_used(),
        reference_outcome.result.logs().len(),
        encode_hex(&log.data.data),
        encode_hex(log.data.topics()[0].as_slice()),
        encode_hex(&slot0.to_be_bytes::<32>()),
        reference_db_accesses.len(),
        dtvm_audit.len(),
        reference_attempts.len(),
    );
}

#[test]
fn dynamic_fee_calldata_value_and_non_allowlisted_return_match_default_revm() {
    let library = verified_dtvm_library();
    let fixture = strict_fixture(false, GASPRICE_CALLDATA_RETURN);
    let mut env = osaka_env();
    env.block_env.basefee = 10;
    let tx = TxEnv {
        tx_type: 2,
        caller: SENDER,
        gas_limit: 120_000,
        gas_price: 30,
        gas_priority_fee: Some(2),
        kind: TxKind::Call(RECIPIENT),
        value: U256::from(123),
        data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        nonce: 0,
        chain_id: Some(1),
        ..Default::default()
    };

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference
        .transact_raw(tx.clone())
        .expect("default REVM dynamic-fee transaction");
    let reference_db_accesses = reference.db().accesses().to_vec();

    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm.transact_raw(tx).expect("DTVM dynamic-fee transaction");

    assert_eq!(dtvm_outcome.result, reference_outcome.result);
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_db_accesses);
    let output = dtvm_outcome.result.output().expect("RETURN output");
    assert_eq!(U256::from_be_slice(&output[..32]), U256::from(12));
    assert_eq!(&output[32..36], &[0xde, 0xad, 0xbe, 0xef]);
    assert!(dtvm
        .last_audit()
        .iter()
        .any(|event| matches!(event, AccessEvent::TxContext)));
}

#[test]
fn revert_result_and_rolled_back_state_match_default_revm() {
    let library = verified_dtvm_library();
    let fixture = strict_fixture(true, STORAGE_REVERT);
    let env = osaka_env();
    let tx = supported_tx();

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference
        .transact_raw(tx.clone())
        .expect("default REVM revert transaction");
    let reference_db_accesses = reference.db().accesses().to_vec();

    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm.transact_raw(tx).expect("DTVM revert transaction");

    assert!(matches!(
        reference_outcome.result,
        ExecutionResult::Revert { .. }
    ));
    assert_eq!(dtvm_outcome.result, reference_outcome.result);
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_db_accesses);
    let slot = &dtvm_outcome.state[&RECIPIENT].storage[&U256::ZERO];
    assert_eq!(slot.present_value, slot.original_value);
    assert_eq!(slot.present_value, U256::ZERO);
}

#[test]
fn type3_blobhash_reads_the_real_evmc_backing_array() {
    let library = verified_dtvm_library();
    let fixture = strict_fixture(false, BLOBHASH_RETURN);
    let env = osaka_env();
    let mut versioned_hash = [0x55; 32];
    versioned_hash[0] = 0x01;
    let versioned_hash = alloy_primitives::B256::from(versioned_hash);
    let tx = TxEnv {
        tx_type: 3,
        caller: SENDER,
        gas_limit: 120_000,
        gas_price: 1,
        kind: TxKind::Call(RECIPIENT),
        nonce: 0,
        chain_id: Some(1),
        blob_hashes: vec![versioned_hash],
        max_fee_per_blob_gas: 1,
        ..Default::default()
    };

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference.transact_raw(tx.clone()).unwrap();
    let reference_accesses = reference.db().accesses().to_vec();
    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm.transact_raw(tx).unwrap();

    assert_eq!(dtvm_outcome.result, reference_outcome.result);
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_accesses);
    assert_eq!(
        dtvm_outcome.result.output().expect("BLOBHASH output"),
        versioned_hash.as_slice()
    );
    assert!(dtvm
        .last_audit()
        .iter()
        .any(|event| matches!(event, AccessEvent::TxContext)));
}

#[test]
fn top_level_create_types_0_through_3_match_reth_address_value_empty_calldata_and_runtime_code() {
    let library = verified_dtvm_library();
    let created = SENDER.create(0);
    let initcode = vec![
        opcode::CALLDATASIZE,
        opcode::PUSH0,
        opcode::MSTORE,
        opcode::PUSH1,
        0x20,
        opcode::PUSH0,
        opcode::RETURN,
    ];
    let runtime = vec![0; 32];

    for tx_type in [0, 1, 2, 3] {
        let fixture = empty_target_fixture(created);
        let env = osaka_env();
        let tx = top_level_create_tx(tx_type, &initcode, 7);
        let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
        let reference_outcome = reference
            .transact_raw(tx.clone())
            .expect("stock Reth top-level CREATE");
        let reference_accesses = reference.db().accesses().to_vec();

        let mut dtvm = DtvmEvmFactory::new(&library).create_evm(fixture, env);
        let dtvm_outcome = dtvm
            .transact_raw(tx)
            .expect("DTVM top-level CREATE initcode");

        assert_eq!(dtvm_outcome.result, reference_outcome.result);
        assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
        assert_eq!(dtvm.db().accesses(), reference_accesses);
        assert!(matches!(
            dtvm_outcome.result,
            ExecutionResult::Success {
                output: revm::context::result::Output::Create(ref output, Some(address)),
                ..
            } if address == created && output.as_ref() == runtime.as_slice()
        ));
        assert_eq!(dtvm_outcome.state[&created].info.balance, U256::from(7));
        assert_eq!(dtvm_outcome.state[&created].info.nonce, 1);
        assert_eq!(
            code_bytes(&dtvm_outcome.state[&created]).as_deref(),
            Some(runtime.as_slice())
        );
        assert_eq!(
            dtvm.last_audit(),
            [AccessEvent::AccountExists(to_dtvm_address(created))],
            "initcode must receive empty calldata and must not validate against runtime code"
        );
    }
}

#[test]
fn top_level_create_revert_and_invalid_code_deposit_match_reth() {
    let library = verified_dtvm_library();
    let created = SENDER.create(0);
    let revert_initcode = [
        opcode::PUSH1,
        0x2a,
        opcode::PUSH0,
        opcode::MSTORE,
        opcode::PUSH1,
        0x20,
        opcode::PUSH0,
        opcode::REVERT,
    ];

    for (initcode, expected_revert) in [
        (revert_initcode.to_vec(), true),
        (initcode_returning(&[0xef]), false),
    ] {
        let fixture = empty_target_fixture(created);
        let env = osaka_env();
        let tx = top_level_create_tx(0, &initcode, 7);
        let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
        let reference_outcome = reference.transact_raw(tx.clone()).unwrap();
        let reference_accesses = reference.db().accesses().to_vec();
        let mut dtvm = DtvmEvmFactory::new(&library).create_evm(fixture, env);
        let dtvm_outcome = dtvm.transact_raw(tx).unwrap();

        assert_eq!(dtvm_outcome.result, reference_outcome.result);
        assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
        assert_eq!(dtvm.db().accesses(), reference_accesses);
        assert_eq!(
            matches!(dtvm_outcome.result, ExecutionResult::Revert { .. }),
            expected_revert
        );
        assert_eq!(
            matches!(dtvm_outcome.result, ExecutionResult::Halt { .. }),
            !expected_revert
        );
        assert_eq!(
            dtvm.last_audit(),
            [AccessEvent::AccountExists(to_dtvm_address(created))]
        );
    }
}

#[test]
fn top_level_create_missing_created_account_witness_fails_closed_before_dtvm() {
    let library = verified_dtvm_library();
    let created = SENDER.create(0);
    let mut fixture = StrictDb::default();
    fixture
        .insert_account(
            SENDER,
            AccountInfo {
                balance: U256::from(10_000_000u64),
                code: None,
                ..Default::default()
            },
        )
        .unwrap();
    fixture.cover_absent_account(BENEFICIARY);
    let env = osaka_env();
    let tx = top_level_create_tx(0, EMPTY_RETURN, 0);

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    assert!(matches!(
        reference.transact_raw(tx.clone()),
        Err(EVMError::Database(StrictDbError::MissingAccount(address)))
            if address == created
    ));
    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    assert!(matches!(
        dtvm.transact_raw(tx),
        Err(EVMError::Database(StrictDbError::MissingAccount(address)))
            if address == created
    ));
    assert!(dtvm.last_audit().is_empty());
}

#[test]
fn empty_code_and_top_level_precompile_complete_inside_reth() {
    let library = verified_dtvm_library();
    let env = osaka_env();
    let tx = supported_tx();

    let fixture = strict_fixture(false, &[]);
    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference.transact_raw(tx.clone()).unwrap();
    let reference_accesses = reference.db().accesses().to_vec();
    let mut dtvm = DtvmEvmFactory::new(&library).create_evm(fixture, env.clone());
    let dtvm_outcome = dtvm.transact_raw(tx.clone()).unwrap();
    assert_eq!(dtvm_outcome.result, reference_outcome.result);
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_accesses);
    assert!(dtvm.last_audit().is_empty());

    let precompile = Address::with_last_byte(1);
    let mut precompile_tx = tx;
    precompile_tx.kind = TxKind::Call(precompile);
    let fixture = empty_target_fixture(precompile);
    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference.transact_raw(precompile_tx.clone()).unwrap();
    let reference_accesses = reference.db().accesses().to_vec();
    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm.transact_raw(precompile_tx).unwrap();
    assert_eq!(dtvm_outcome.result, reference_outcome.result);
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_accesses);
    assert!(dtvm.last_audit().is_empty());
}

#[test]
fn evmc_success_is_return_because_evmc_cannot_report_stop_vs_return() {
    let library = verified_dtvm_library();
    let fixture = strict_fixture(false, &[opcode::STOP]);
    let env = osaka_env();
    let tx = supported_tx();

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference.transact_raw(tx.clone()).unwrap();
    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm.transact_raw(tx).unwrap();

    assert!(matches!(
        reference_outcome.result,
        ExecutionResult::Success {
            reason: SuccessReason::Stop,
            ..
        }
    ));
    assert!(matches!(
        dtvm_outcome.result,
        ExecutionResult::Success {
            reason: SuccessReason::Return,
            ..
        }
    ));
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
}

#[test]
fn execute_evm_replay_uses_current_transaction_and_finalizes() {
    let library = verified_dtvm_library();
    let fixture = strict_fixture(false, GASPRICE_CALLDATA_RETURN);
    let env = osaka_env();
    let tx = TxEnv {
        tx_type: 2,
        caller: SENDER,
        gas_limit: 120_000,
        gas_price: 3,
        gas_priority_fee: Some(1),
        kind: TxKind::Call(RECIPIENT),
        data: Bytes::from_static(&[0x01]),
        nonce: 0,
        chain_id: Some(1),
        ..Default::default()
    };

    let mut reference = EthEvmFactory::default()
        .create_evm(fixture.clone(), env.clone())
        .into_inner();
    reference.all_mut().0.tx = tx.clone();
    let reference_outcome = ExecuteEvm::replay(&mut reference).unwrap();
    let reference_accesses = reference
        .all()
        .0
        .journaled_state
        .database
        .accesses()
        .to_vec();

    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    dtvm.all_mut().0.tx = tx;
    let dtvm_outcome = ExecuteEvm::replay(&mut dtvm).unwrap();

    assert_eq!(dtvm_outcome.result, reference_outcome.result);
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_accesses);
}

#[test]
fn history_shaped_system_call_matches_reth_with_absent_caller() {
    let library = verified_dtvm_library();
    let fixture = system_fixture(&HISTORY_STORAGE_RETURN_CODE, true, false);
    let mut env = osaka_env();
    env.block_env.number = U256::ONE;
    let block_hash = B256::repeat_byte(0x11);
    let data = Bytes::copy_from_slice(block_hash.as_slice());

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference
        .transact_system_call(SYSTEM_ADDRESS, HISTORY_STORAGE_ADDRESS, data.clone())
        .expect("stock Reth system-call shell");
    let reference_accesses = reference.db().accesses().to_vec();

    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm
        .transact_system_call(SYSTEM_ADDRESS, HISTORY_STORAGE_ADDRESS, data)
        .expect("DTVM-backed system-call shell");

    assert_eq!(dtvm_outcome.result, reference_outcome.result);
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_accesses);
    assert_eq!(
        dtvm_outcome.state[&HISTORY_STORAGE_ADDRESS].storage[&U256::ZERO].present_value,
        U256::from_be_bytes(block_hash.0)
    );
    assert!(
        !reference_accesses
            .iter()
            .any(|access| matches!(access, DbAccess::Basic(address) if *address == SYSTEM_ADDRESS || *address == BENEFICIARY)),
        "system shell must neither load/deduct caller nor reward beneficiary"
    );
    assert!(
        !dtvm
            .last_audit()
            .contains(&AccessEvent::AccountExists(to_dtvm_address(SYSTEM_ADDRESS))),
        "VM validation must not redundantly validate the system caller"
    );
}

#[test]
fn nested_nonempty_call_matches_reth_for_storage_log_and_output() {
    let root_code = call_then_return(CHILD, 32);
    let mut fixture = strict_fixture(false, &root_code);
    insert_contract(&mut fixture, CHILD, 8, STORAGE_LOG_RETURN, U256::ZERO);
    fixture
        .cover_storage(CHILD, U256::ZERO, U256::ZERO)
        .unwrap();

    let (result, state, audit) = assert_nested_diff(&root_code, fixture);
    assert!(matches!(
        result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if output.last() == Some(&0x2a)
    ));
    assert_eq!(
        state[&CHILD].storage[&U256::ZERO].present_value,
        U256::from(42)
    );
    assert!(audit
        .iter()
        .any(|event| matches!(event, AccessEvent::NestedCall(0))));
    assert!(audit.iter().any(
        |event| matches!(event, AccessEvent::StorageWrite(address, ..) if *address == to_dtvm_address(CHILD))
    ));
}

#[test]
fn nested_revert_and_halt_roll_back_child_writes_while_parent_continues() {
    for (child_code, expected_last_byte) in [
        (STORAGE_REVERT_WITH_OUTPUT, Some(0x2a)),
        (STORAGE_HALT, Some(0x00)),
    ] {
        let mut root_code = Vec::new();
        append_ordinary_call(&mut root_code, CHILD, 0, 0, 0, 0, 32);
        root_code.push(opcode::POP);
        root_code.extend_from_slice(&[
            opcode::PUSH1,
            0x77,
            opcode::PUSH1,
            0x01,
            opcode::SSTORE,
            opcode::PUSH1,
            0x20,
            opcode::PUSH0,
            opcode::RETURN,
        ]);
        let mut fixture = strict_fixture(false, &root_code);
        insert_contract(&mut fixture, CHILD, 8, child_code, U256::ZERO);
        fixture
            .cover_storage(RECIPIENT, U256::ONE, U256::ZERO)
            .unwrap();
        fixture
            .cover_storage(CHILD, U256::ZERO, U256::ZERO)
            .unwrap();

        let (result, state, _) = assert_nested_diff(&root_code, fixture);
        assert!(matches!(
            result,
            ExecutionResult::Success {
                output: revm::context::result::Output::Call(ref output),
                ..
            } if output.last() == expected_last_byte.as_ref()
        ));
        assert_eq!(
            state[&CHILD].storage[&U256::ZERO].present_value,
            U256::ZERO,
            "child write must revert for {}",
            encode_hex(child_code)
        );
        assert_eq!(
            state[&RECIPIENT].storage[&U256::ONE].present_value,
            U256::from(0x77)
        );
    }
}

#[test]
fn nested_empty_and_identity_precompile_match_reth() {
    let empty_root = call_then_return(EMPTY_CHILD, 0);
    let mut empty_fixture = strict_fixture(false, &empty_root);
    empty_fixture.cover_absent_account(EMPTY_CHILD);
    let (empty_result, _, empty_audit) = assert_nested_diff(&empty_root, empty_fixture);
    assert!(matches!(empty_result, ExecutionResult::Success { .. }));
    assert!(empty_audit
        .iter()
        .any(|event| matches!(event, AccessEvent::NestedCall(0))));

    let mut identity_root = vec![opcode::PUSH1, 0x2a, opcode::PUSH0, opcode::MSTORE];
    append_ordinary_call(
        &mut identity_root,
        Address::with_last_byte(4),
        0,
        31,
        1,
        0,
        1,
    );
    identity_root.push(opcode::POP);
    identity_root.extend_from_slice(&[opcode::PUSH1, 0x01, opcode::PUSH0, opcode::RETURN]);
    let mut identity_fixture = strict_fixture(false, &identity_root);
    identity_fixture.cover_absent_account(Address::with_last_byte(4));
    let (identity_result, _, _) = assert_nested_diff(&identity_root, identity_fixture);
    assert!(matches!(
        identity_result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if output.as_ref() == [0x2a]
    ));
}

#[test]
fn nested_value_transfer_commits_on_success_and_reverts_with_child() {
    for (child_code, expected_parent, expected_child) in [
        (EMPTY_RETURN, U256::from(93), U256::from(12)),
        (EMPTY_REVERT, U256::from(100), U256::from(5)),
    ] {
        let mut root_code = Vec::new();
        append_ordinary_call(&mut root_code, CHILD, 7, 0, 0, 0, 0);
        root_code.extend_from_slice(&[opcode::POP, opcode::PUSH0, opcode::PUSH0, opcode::RETURN]);
        let mut fixture = strict_fixture(false, &root_code);
        insert_contract(&mut fixture, RECIPIENT, 7, &root_code, U256::from(100));
        insert_contract(&mut fixture, CHILD, 8, child_code, U256::from(5));

        let (result, state, _) = assert_nested_diff(&root_code, fixture);
        assert!(matches!(result, ExecutionResult::Success { .. }));
        assert_eq!(state[&RECIPIENT].info.balance, expected_parent);
        assert_eq!(state[&CHILD].info.balance, expected_child);
    }
}

#[test]
fn nested_staticcall_read_and_state_change_violations_match_reth() {
    let read_root = staticcall_then_return(CHILD, 32);
    let mut read_fixture = strict_fixture(false, &read_root);
    insert_contract(&mut read_fixture, CHILD, 8, STORAGE_READ_RETURN, U256::ZERO);
    read_fixture
        .cover_storage(CHILD, U256::ZERO, U256::from(42))
        .unwrap();

    let (result, _, audit) = assert_nested_diff(&read_root, read_fixture);
    assert!(matches!(
        result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if output.last() == Some(&0x2a)
    ));
    assert!(audit
        .iter()
        .any(|event| matches!(event, AccessEvent::NestedCall(0))));

    for child_code in [STORAGE_WRITE_STOP, LOG_STOP] {
        let mut root = Vec::new();
        append_staticcall(&mut root, CHILD, 0, 0, 0, 0);
        root.extend_from_slice(&[
            opcode::PUSH0,
            opcode::MSTORE,
            opcode::PUSH1,
            0x20,
            opcode::PUSH0,
            opcode::RETURN,
        ]);
        let mut fixture = strict_fixture(false, &root);
        insert_contract(&mut fixture, CHILD, 8, child_code, U256::ZERO);
        if child_code == STORAGE_WRITE_STOP {
            fixture
                .cover_storage(CHILD, U256::ZERO, U256::ZERO)
                .unwrap();
        }

        let (result, state, _) = assert_nested_diff(&root, fixture);
        assert!(matches!(
            result,
            ExecutionResult::Success {
                output: revm::context::result::Output::Call(ref output),
                ..
            } if U256::from_be_slice(output) == U256::ZERO
        ));
        if child_code == STORAGE_WRITE_STOP {
            assert_eq!(
                state
                    .get(&CHILD)
                    .and_then(|account| account.storage.get(&U256::ZERO))
                    .map_or(U256::ZERO, |slot| slot.present_value),
                U256::ZERO
            );
        }
    }
}

#[test]
fn nested_delegatecall_uses_parent_storage_caller_and_value() {
    let mut root = Vec::new();
    append_delegatecall(&mut root, CHILD, 0, 0, 0, 0);
    root.extend_from_slice(&[opcode::POP, opcode::PUSH0, opcode::PUSH0, opcode::RETURN]);
    let mut fixture = strict_fixture(false, &root);
    insert_contract(&mut fixture, CHILD, 8, CONTEXT_STORE, U256::ZERO);
    for slot in 0..=2 {
        fixture
            .cover_storage(RECIPIENT, U256::from(slot), U256::ZERO)
            .unwrap();
    }
    let mut tx = supported_tx();
    tx.value = U256::from(7);

    let (result, state, audit) = assert_nested_diff_with_tx(&root, fixture, tx);
    assert!(matches!(result, ExecutionResult::Success { .. }));
    assert_eq!(
        state[&RECIPIENT].storage[&U256::ZERO].present_value,
        U256::from_be_slice(RECIPIENT.as_slice())
    );
    assert_eq!(
        state[&RECIPIENT].storage[&U256::ONE].present_value,
        U256::from_be_slice(SENDER.as_slice())
    );
    assert_eq!(
        state[&RECIPIENT].storage[&U256::from(2)].present_value,
        U256::from(7)
    );
    assert!(audit
        .iter()
        .any(|event| matches!(event, AccessEvent::NestedCall(1))));
}

#[test]
fn nested_callcode_uses_parent_storage_caller_and_explicit_value() {
    let mut root = Vec::new();
    append_callcode(&mut root, CHILD, 5, 0, 0, 0, 0);
    root.extend_from_slice(&[opcode::POP, opcode::PUSH0, opcode::PUSH0, opcode::RETURN]);
    let mut fixture = strict_fixture(false, &root);
    insert_contract(&mut fixture, RECIPIENT, 7, &root, U256::from(10));
    insert_contract(&mut fixture, CHILD, 8, CONTEXT_STORE, U256::ZERO);
    for slot in 0..=2 {
        fixture
            .cover_storage(RECIPIENT, U256::from(slot), U256::ZERO)
            .unwrap();
    }

    let (result, state, audit) = assert_nested_diff(&root, fixture);
    assert!(matches!(result, ExecutionResult::Success { .. }));
    for slot in [U256::ZERO, U256::ONE] {
        assert_eq!(
            state[&RECIPIENT].storage[&slot].present_value,
            U256::from_be_slice(RECIPIENT.as_slice())
        );
    }
    assert_eq!(
        state[&RECIPIENT].storage[&U256::from(2)].present_value,
        U256::from(5)
    );
    assert_eq!(state[&RECIPIENT].info.balance, U256::from(10));
    assert!(audit
        .iter()
        .any(|event| matches!(event, AccessEvent::NestedCall(2))));
}

#[test]
fn nested_create_and_create2_deploy_with_reth_address_value_and_calldata_semantics() {
    let runtime = LOW_ADDRESS_RETURN.to_vec();
    let create_initcode = initcode_returning(&runtime);
    let create2_initcode = vec![
        opcode::CALLDATASIZE,
        opcode::PUSH0,
        opcode::MSTORE,
        opcode::PUSH1,
        0x20,
        opcode::PUSH0,
        opcode::RETURN,
    ];
    for (create2, salt, initcode, expected_code, event_kind) in [
        (false, 0, create_initcode, runtime, 3),
        (true, 7, create2_initcode, vec![0; 32], 4),
    ] {
        let root = create_then_return(&initcode, create2, 5, salt);
        let created = if create2 {
            RECIPIENT.create2(U256::from(salt).to_be_bytes(), keccak256(&initcode))
        } else {
            RECIPIENT.create(1)
        };
        let mut fixture = strict_fixture(false, &root);
        insert_contract(&mut fixture, RECIPIENT, 7, &root, U256::from(10));
        fixture.cover_absent_account(created);

        let (result, state, audit) = assert_nested_diff(&root, fixture);
        assert!(matches!(
            result,
            ExecutionResult::Success {
                output: revm::context::result::Output::Call(ref output),
                ..
            } if U256::from_be_slice(output) == U256::from_be_slice(created.as_slice())
        ));
        assert_eq!(state[&RECIPIENT].info.balance, U256::from(5));
        assert_eq!(state[&RECIPIENT].info.nonce, 2);
        assert_eq!(state[&created].info.balance, U256::from(5));
        assert_eq!(
            code_bytes(&state[&created]).as_deref(),
            Some(expected_code.as_slice())
        );
        assert!(audit
            .iter()
            .any(|event| matches!(event, AccessEvent::NestedCall(kind) if *kind == event_kind)));
    }
}

#[test]
fn nested_create2_revert_rolls_back_created_state_and_value_but_keeps_creator_nonce() {
    let initcode = vec![
        opcode::PUSH1,
        0x2a,
        opcode::PUSH0,
        opcode::SSTORE,
        opcode::PUSH1,
        0x2a,
        opcode::PUSH0,
        opcode::MSTORE,
        opcode::PUSH1,
        0x20,
        opcode::PUSH0,
        opcode::REVERT,
    ];
    let root = create_then_return(&initcode, true, 5, 9);
    let created = RECIPIENT.create2(U256::from(9).to_be_bytes(), keccak256(&initcode));
    let mut fixture = strict_fixture(false, &root);
    insert_contract(&mut fixture, RECIPIENT, 7, &root, U256::from(10));
    fixture.cover_absent_account(created);

    let (result, state, _) = assert_nested_diff(&root, fixture);
    assert!(matches!(
        result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if U256::from_be_slice(output) == U256::ZERO
    ));
    assert_eq!(state[&RECIPIENT].info.balance, U256::from(10));
    assert_eq!(state[&RECIPIENT].info.nonce, 2);
    if let Some(account) = state.get(&created) {
        assert_eq!(account.info.balance, U256::ZERO);
        assert_eq!(account.info.code_hash, keccak256([]));
        assert!(account
            .storage
            .values()
            .all(|slot| slot.present_value == U256::ZERO));
    }
}

#[test]
fn nested_create2_collision_and_insufficient_balance_match_reth_light_failure() {
    let initcode = initcode_returning(LOW_ADDRESS_RETURN);
    let created = RECIPIENT.create2(U256::from(11).to_be_bytes(), keccak256(&initcode));
    let root = create_then_return(&initcode, true, 0, 11);
    let mut collision_fixture = strict_fixture(false, &root);
    insert_contract(&mut collision_fixture, RECIPIENT, 7, &root, U256::from(10));
    insert_contract(
        &mut collision_fixture,
        created,
        8,
        EMPTY_RETURN,
        U256::from(3),
    );

    let (result, state, _) = assert_nested_diff(&root, collision_fixture);
    assert!(matches!(
        result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if U256::from_be_slice(output) == U256::ZERO
    ));
    assert_eq!(state[&RECIPIENT].info.nonce, 2);
    assert_eq!(state[&created].info.balance, U256::from(3));
    assert_eq!(state[&created].info.code_hash, keccak256(EMPTY_RETURN));

    let insufficient_root = create_then_return(&initcode, true, 1, 12);
    let mut insufficient_fixture = strict_fixture(false, &insufficient_root);
    insert_contract(
        &mut insufficient_fixture,
        RECIPIENT,
        7,
        &insufficient_root,
        U256::ZERO,
    );
    let (result, state, audit) = assert_nested_diff(&insufficient_root, insufficient_fixture);
    assert!(matches!(
        result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if U256::from_be_slice(output) == U256::ZERO
    ));
    assert_eq!(state[&RECIPIENT].info.nonce, 1);
    assert!(
        !audit
            .iter()
            .any(|event| matches!(event, AccessEvent::NestedCall(4))),
        "DTVM must perform CREATE2's balance light-failure before calling the host"
    );
}

#[test]
fn create2_inside_staticcall_halts_child_and_preserves_state() {
    let initcode = initcode_returning(LOW_ADDRESS_RETURN);
    let child_code = create_then_return(&initcode, true, 0, 13);
    let mut root = Vec::new();
    append_staticcall(&mut root, CHILD, 0, 0, 0, 0);
    root.extend_from_slice(&[
        opcode::PUSH0,
        opcode::MSTORE,
        opcode::PUSH1,
        0x20,
        opcode::PUSH0,
        opcode::RETURN,
    ]);
    let mut fixture = strict_fixture(false, &root);
    insert_contract(&mut fixture, CHILD, 8, &child_code, U256::ZERO);

    let (result, state, audit) = assert_nested_diff(&root, fixture);
    assert!(matches!(
        result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if U256::from_be_slice(output) == U256::ZERO
    ));
    assert_eq!(state[&CHILD].info.nonce, 1);
    assert!(
        !audit
            .iter()
            .any(|event| matches!(event, AccessEvent::NestedCall(4))),
        "static CREATE2 must halt before the host callback"
    );
}

#[test]
fn osaka_existing_contract_selfdestruct_transfers_balance_but_keeps_code_and_nonce() {
    let child_code = selfdestruct_code(EMPTY_CHILD);
    let mut root = Vec::new();
    append_ordinary_call(&mut root, CHILD, 0, 0, 0, 0, 0);
    root.extend_from_slice(&[opcode::POP, opcode::PUSH0, opcode::PUSH0, opcode::RETURN]);
    let mut fixture = strict_fixture(false, &root);
    insert_contract(&mut fixture, CHILD, 8, &child_code, U256::from(7));
    fixture.cover_absent_account(EMPTY_CHILD);

    let (result, state, audit) = assert_nested_diff(&root, fixture);
    assert!(matches!(result, ExecutionResult::Success { .. }));
    assert_eq!(state[&CHILD].info.balance, U256::ZERO);
    assert_eq!(state[&CHILD].info.nonce, 1);
    assert_eq!(state[&CHILD].info.code_hash, keccak256(&child_code));
    assert!(!state[&CHILD].is_selfdestructed());
    assert_eq!(state[&EMPTY_CHILD].info.balance, U256::from(7));
    assert!(audit.iter().any(
        |event| matches!(event, AccessEvent::Selfdestruct(address, beneficiary)
            if *address == to_dtvm_address(CHILD)
                && *beneficiary == to_dtvm_address(EMPTY_CHILD))
    ));
}

#[test]
fn create2_initcode_selfdestruct_removes_created_contract_and_transfers_value() {
    let initcode = selfdestruct_code(EMPTY_CHILD);
    let root = create_then_return(&initcode, true, 5, 14);
    let created = RECIPIENT.create2(U256::from(14).to_be_bytes(), keccak256(&initcode));
    let mut fixture = strict_fixture(false, &root);
    insert_contract(&mut fixture, RECIPIENT, 7, &root, U256::from(10));
    fixture.cover_absent_account(created);
    fixture.cover_absent_account(EMPTY_CHILD);

    let (result, state, audit) = assert_nested_diff(&root, fixture);
    assert!(matches!(
        result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if U256::from_be_slice(output) == U256::from_be_slice(created.as_slice())
    ));
    assert_eq!(state[&RECIPIENT].info.balance, U256::from(5));
    assert_eq!(state[&RECIPIENT].info.nonce, 2);
    assert_eq!(state[&EMPTY_CHILD].info.balance, U256::from(5));
    assert!(state[&created].is_selfdestructed());
    assert_eq!(state[&created].info.balance, U256::ZERO);
    assert!(audit.iter().any(
        |event| matches!(event, AccessEvent::Selfdestruct(address, beneficiary)
            if *address == to_dtvm_address(created)
                && *beneficiary == to_dtvm_address(EMPTY_CHILD))
    ));
}

#[test]
fn reverted_child_rolls_back_nested_selfdestruct_transfer() {
    let grandchild_code = selfdestruct_code(EMPTY_CHILD);
    let mut child_code = Vec::new();
    append_ordinary_call(&mut child_code, GRANDCHILD, 0, 0, 0, 0, 0);
    child_code.extend_from_slice(&[opcode::POP, opcode::PUSH0, opcode::PUSH0, opcode::REVERT]);
    let mut root = Vec::new();
    append_ordinary_call(&mut root, CHILD, 0, 0, 0, 0, 0);
    root.extend_from_slice(&[opcode::POP, opcode::PUSH0, opcode::PUSH0, opcode::RETURN]);
    let mut fixture = strict_fixture(false, &root);
    insert_contract(&mut fixture, CHILD, 8, &child_code, U256::ZERO);
    insert_contract(&mut fixture, GRANDCHILD, 9, &grandchild_code, U256::from(7));
    fixture.cover_absent_account(EMPTY_CHILD);

    let (result, state, audit) = assert_nested_diff(&root, fixture);
    assert!(matches!(result, ExecutionResult::Success { .. }));
    assert_eq!(state[&GRANDCHILD].info.balance, U256::from(7));
    assert_eq!(
        state[&GRANDCHILD].info.code_hash,
        keccak256(&grandchild_code)
    );
    assert!(!state[&GRANDCHILD].is_selfdestructed());
    assert!(state
        .get(&EMPTY_CHILD)
        .is_none_or(|account| account.info.balance == U256::ZERO));
    assert!(audit.iter().any(
        |event| matches!(event, AccessEvent::Selfdestruct(address, beneficiary)
            if *address == to_dtvm_address(GRANDCHILD)
                && *beneficiary == to_dtvm_address(EMPTY_CHILD))
    ));
}

#[test]
fn repeated_same_target_selfdestruct_keeps_existing_contract_unchanged() {
    let child_code = selfdestruct_code(CHILD);
    let mut root = Vec::new();
    for _ in 0..2 {
        append_ordinary_call(&mut root, CHILD, 0, 0, 0, 0, 0);
        root.push(opcode::POP);
    }
    root.extend_from_slice(&[opcode::PUSH0, opcode::PUSH0, opcode::RETURN]);
    let mut fixture = strict_fixture(false, &root);
    insert_contract(&mut fixture, CHILD, 8, &child_code, U256::from(7));

    let (result, state, audit) = assert_nested_diff(&root, fixture);
    assert!(matches!(result, ExecutionResult::Success { .. }));
    assert_eq!(state[&CHILD].info.balance, U256::from(7));
    assert_eq!(state[&CHILD].info.nonce, 1);
    assert_eq!(state[&CHILD].info.code_hash, keccak256(&child_code));
    assert!(!state[&CHILD].is_selfdestructed());
    assert_eq!(
        audit
            .iter()
            .filter(
                |event| matches!(event, AccessEvent::Selfdestruct(address, beneficiary)
                if *address == to_dtvm_address(CHILD)
                    && *beneficiary == to_dtvm_address(CHILD))
            )
            .count(),
        2
    );
}

#[test]
fn selfdestruct_missing_beneficiary_witness_fails_closed_and_clears_journal() {
    let code = selfdestruct_code(EMPTY_CHILD);
    let mut fixture = strict_fixture(false, &code);
    insert_contract(&mut fixture, RECIPIENT, 7, &code, U256::from(7));
    let env = osaka_env();
    let tx = supported_tx();

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    assert!(matches!(
        reference.transact_raw(tx.clone()),
        Err(EVMError::Database(StrictDbError::MissingAccount(address)))
            if address == EMPTY_CHILD
    ));

    let mut dtvm = DtvmEvmFactory::new(verified_dtvm_library()).create_evm(fixture, env);
    assert!(matches!(
        dtvm.transact_raw(tx),
        Err(EVMError::Database(StrictDbError::MissingAccount(address)))
            if address == EMPTY_CHILD
    ));
    assert!(dtvm.db().accesses().contains(&DbAccess::Basic(EMPTY_CHILD)));
    assert!(!dtvm.last_audit().iter().any(
        |event| matches!(event, AccessEvent::Selfdestruct(_, beneficiary)
            if *beneficiary == to_dtvm_address(EMPTY_CHILD))
    ));
    let journal = &dtvm.all().0.journaled_state.inner;
    assert!(journal.journal.is_empty());
    assert!(journal.logs.is_empty());
    if let Some(sender) = journal.state.get(&SENDER) {
        assert_eq!(sender.info.nonce, 0);
        assert_eq!(sender.info.balance, U256::from(10_000_000u64));
    }
}

#[test]
fn existing_eip7702_delegation_executes_in_recipient_context() {
    let mut fixture = strict_fixture(false, EMPTY_RETURN);
    let delegation = Bytecode::new_eip7702(CHILD);
    let delegation_hash = keccak256(delegation.original_bytes());
    fixture
        .insert_account(
            RECIPIENT,
            AccountInfo {
                nonce: 1,
                code_hash: delegation_hash,
                account_id: Some(AccountId::new(7).unwrap()),
                code: None,
                ..Default::default()
            },
        )
        .unwrap();
    fixture.insert_code(delegation_hash, delegation).unwrap();
    insert_contract(&mut fixture, CHILD, 8, STORAGE_WRITE_RETURN, U256::ZERO);
    fixture
        .cover_storage(RECIPIENT, U256::ZERO, U256::ZERO)
        .unwrap();

    let (result, state, _) = assert_nested_diff(EMPTY_RETURN, fixture);
    assert!(matches!(result, ExecutionResult::Success { .. }));
    assert_eq!(
        state[&RECIPIENT].storage[&U256::ZERO].present_value,
        U256::from(42)
    );
}

#[test]
fn nested_eip7702_delegation_executes_delegate_code_in_authority_context() {
    let root_code = call_then_return(CHILD, 0);
    let mut fixture = strict_fixture(false, &root_code);
    insert_delegation(&mut fixture, CHILD, 8, GRANDCHILD);
    insert_contract(
        &mut fixture,
        GRANDCHILD,
        9,
        STORAGE_WRITE_RETURN,
        U256::ZERO,
    );
    fixture
        .cover_storage(CHILD, U256::ZERO, U256::ZERO)
        .unwrap();

    let (result, state, audit) = assert_nested_diff(&root_code, fixture);

    assert!(matches!(result, ExecutionResult::Success { .. }));
    assert_eq!(
        state[&CHILD].storage[&U256::ZERO].present_value,
        U256::from(42)
    );
    assert!(!state
        .get(&GRANDCHILD)
        .is_some_and(|account| account.storage.contains_key(&U256::ZERO)));
    assert!(audit.iter().any(
        |event| matches!(event, AccessEvent::StorageWrite(address, ..)
            if *address == to_dtvm_address(CHILD))
    ));
}

#[test]
fn nested_chained_eip7702_delegation_executes_only_first_delegate_designation() {
    let root_code = call_then_return(CHILD, 0);
    let mut fixture = strict_fixture(false, &root_code);
    insert_delegation(&mut fixture, CHILD, 8, GRANDCHILD);
    insert_delegation(&mut fixture, GRANDCHILD, 9, EMPTY_CHILD);
    insert_contract(
        &mut fixture,
        EMPTY_CHILD,
        10,
        STORAGE_WRITE_RETURN,
        U256::ZERO,
    );
    fixture
        .cover_storage(CHILD, U256::ZERO, U256::ZERO)
        .unwrap();

    let (result, state, audit) = assert_nested_diff(&root_code, fixture);

    assert!(matches!(result, ExecutionResult::Success { .. }));
    assert!(
        !state
            .get(&CHILD)
            .is_some_and(|account| account.storage.contains_key(&U256::ZERO)),
        "the second-hop delegate code must not execute"
    );
    assert!(!audit.iter().any(
        |event| matches!(event, AccessEvent::StorageWrite(address, ..)
            if *address == to_dtvm_address(CHILD))
    ));
    assert!(!audit.iter().any(|event| match event {
        AccessEvent::AccountExists(address)
        | AccessEvent::Balance(address)
        | AccessEvent::CodeSize(address)
        | AccessEvent::CodeHash(address)
        | AccessEvent::CodeCopy(address, ..)
        | AccessEvent::AccountWarm(address)
            if *address == to_dtvm_address(EMPTY_CHILD) =>
        {
            true
        }
        _ => false,
    }));
}

#[test]
fn type4_authorization_delegates_and_executes_in_authority_context() {
    let mut fixture = authority_fixture(RECIPIENT, 0, None);
    insert_contract(&mut fixture, CHILD, 8, CONTEXT_STORE, U256::ZERO);
    for key in 0..=2 {
        fixture
            .cover_storage(RECIPIENT, U256::from(key), U256::ZERO)
            .unwrap();
    }
    let tx = type4_tx(
        RECIPIENT,
        vec![recovered_authorization(1, CHILD, 0, Some(RECIPIENT))],
        7,
    );

    let (result, state, audit) = assert_nested_diff_with_tx(CONTEXT_STORE, fixture, tx);

    assert!(matches!(result, ExecutionResult::Success { .. }));
    let authority = &state[&RECIPIENT];
    assert_eq!(authority.info.nonce, 1);
    assert_eq!(
        code_bytes(authority),
        Some(Bytecode::new_eip7702(CHILD).original_bytes().to_vec())
    );
    assert_eq!(
        authority.storage[&U256::ZERO].present_value,
        U256::from_be_slice(RECIPIENT.as_slice())
    );
    assert_eq!(
        authority.storage[&U256::from(1)].present_value,
        U256::from_be_slice(SENDER.as_slice())
    );
    assert_eq!(
        authority.storage[&U256::from(2)].present_value,
        U256::from(7)
    );
    assert!(audit.contains(&AccessEvent::AccountExists(to_dtvm_address(RECIPIENT))));
    assert!(audit.contains(&AccessEvent::AccountExists(to_dtvm_address(CHILD))));
    assert!(audit.iter().any(
        |event| matches!(event, AccessEvent::StorageWrite(address, ..)
            if *address == to_dtvm_address(RECIPIENT))
    ));
}

#[test]
fn type4_invalid_chain_nonce_and_recovery_authorizations_are_skipped() {
    let mut fixture = strict_fixture(false, EMPTY_RETURN);
    fixture
        .insert_account(
            CHILD,
            AccountInfo {
                account_id: Some(AccountId::new(8).unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
    let tx = type4_tx(
        RECIPIENT,
        vec![
            recovered_authorization(2, GRANDCHILD, 0, Some(CHILD)),
            recovered_authorization(1, GRANDCHILD, 1, Some(CHILD)),
            recovered_authorization(1, GRANDCHILD, 0, None),
        ],
        0,
    );

    let (result, state, _) = assert_nested_diff_with_tx(EMPTY_RETURN, fixture, tx);

    assert!(matches!(result, ExecutionResult::Success { .. }));
    if let Some(authority) = state.get(&CHILD) {
        assert_eq!(authority.info.nonce, 0);
        assert!(authority.info.code.as_ref().is_none_or(Bytecode::is_empty));
    }
    assert!(
        !state.contains_key(&GRANDCHILD),
        "skipped authorizations must not load or delegate to their target"
    );
}

#[test]
fn type4_zero_address_authorization_clears_existing_delegation() {
    let delegation = Bytecode::new_eip7702(CHILD);
    let fixture = authority_fixture(RECIPIENT, 1, Some(delegation));
    let tx = type4_tx(
        RECIPIENT,
        vec![recovered_authorization(
            1,
            Address::ZERO,
            1,
            Some(RECIPIENT),
        )],
        0,
    );

    let (result, state, audit) = assert_nested_diff_with_tx(&[], fixture, tx);

    assert!(matches!(result, ExecutionResult::Success { .. }));
    let authority = &state[&RECIPIENT];
    assert_eq!(authority.info.nonce, 2);
    assert_eq!(authority.info.code_hash, keccak256([]));
    assert!(authority.info.code.as_ref().is_none_or(Bytecode::is_empty));
    assert!(
        audit.is_empty(),
        "cleared delegation leaves no bytecode frame for DTVM"
    );
}

#[test]
fn type4_authorization_persists_when_delegated_execution_reverts() {
    let mut fixture = authority_fixture(RECIPIENT, 0, None);
    insert_contract(&mut fixture, CHILD, 8, EMPTY_REVERT, U256::ZERO);
    let tx = type4_tx(
        RECIPIENT,
        vec![recovered_authorization(1, CHILD, 0, Some(RECIPIENT))],
        0,
    );

    let (result, state, audit) = assert_nested_diff_with_tx(EMPTY_REVERT, fixture, tx);

    assert!(matches!(result, ExecutionResult::Revert { .. }));
    let authority = &state[&RECIPIENT];
    assert_eq!(authority.info.nonce, 1);
    assert_eq!(
        code_bytes(authority),
        Some(Bytecode::new_eip7702(CHILD).original_bytes().to_vec())
    );
    assert!(audit.contains(&AccessEvent::AccountExists(to_dtvm_address(CHILD))));
}

#[test]
fn type4_missing_authority_or_delegate_witness_fails_closed_before_dtvm() {
    let mut missing_authority = StrictDb::default();
    missing_authority
        .insert_account(
            SENDER,
            AccountInfo {
                balance: U256::from(10_000_000u64),
                ..Default::default()
            },
        )
        .unwrap();
    missing_authority.cover_absent_account(BENEFICIARY);

    let missing_delegate = authority_fixture(RECIPIENT, 0, None);

    for (fixture, expected) in [
        (missing_authority, StrictDbError::MissingAccount(RECIPIENT)),
        (missing_delegate, StrictDbError::MissingAccount(CHILD)),
    ] {
        let library = verified_dtvm_library();
        let env = osaka_env();
        let tx = type4_tx(
            RECIPIENT,
            vec![recovered_authorization(1, CHILD, 0, Some(RECIPIENT))],
            0,
        );
        let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
        let reference_error = reference
            .transact_raw(tx.clone())
            .expect_err("stock Reth must reject an incomplete EIP-7702 witness");
        let reference_accesses = reference.db().accesses().to_vec();
        let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
        let dtvm_error = dtvm
            .transact_raw(tx)
            .expect_err("DTVM shell must reject an incomplete EIP-7702 witness");

        match reference_error {
            EVMError::Database(error) => assert_eq!(error, expected),
            other => panic!("unexpected stock Reth EIP-7702 error: {other}"),
        }
        match dtvm_error {
            EVMError::Database(error) => assert_eq!(error, expected),
            other => panic!("unexpected DTVM EIP-7702 error: {other}"),
        }
        assert_eq!(dtvm.db().accesses(), reference_accesses);
        assert!(dtvm.last_audit().is_empty());
    }
}

#[test]
fn two_levels_of_nonempty_dtvm_recursion_match_reth() {
    let child_code = call_then_return(GRANDCHILD, 32);
    let root_code = call_then_return(CHILD, 32);
    let mut fixture = strict_fixture(false, &root_code);
    insert_contract(&mut fixture, CHILD, 8, &child_code, U256::ZERO);
    insert_contract(&mut fixture, GRANDCHILD, 9, STORAGE_LOG_RETURN, U256::ZERO);
    fixture
        .cover_storage(GRANDCHILD, U256::ZERO, U256::ZERO)
        .unwrap();

    let (result, state, audit) = assert_nested_diff(&root_code, fixture);
    assert!(matches!(
        result,
        ExecutionResult::Success {
            output: revm::context::result::Output::Call(ref output),
            ..
        } if output.last() == Some(&0x2a)
    ));
    assert_eq!(
        state[&GRANDCHILD].storage[&U256::ZERO].present_value,
        U256::from(42)
    );
    assert_eq!(
        audit
            .iter()
            .filter(|event| matches!(event, AccessEvent::NestedCall(0)))
            .count(),
        2
    );
}

#[test]
fn failed_system_call_fully_rolls_back_and_finalizes() {
    let library = verified_dtvm_library();
    let mut code = vec![opcode::PUSH1, 0x2a, opcode::PUSH0, opcode::SSTORE];
    code.extend_from_slice(&[
        opcode::PUSH0,
        opcode::PUSH0,
        opcode::PUSH0,
        opcode::CREATE,
        opcode::STOP,
    ]);
    let fixture = system_fixture(&code, true, false);
    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, osaka_env());

    let error = dtvm
        .transact_system_call(SYSTEM_ADDRESS, HISTORY_STORAGE_ADDRESS, Bytes::new())
        .expect_err("CREATE with a missing created-account witness must fail closed");
    assert!(
        error.to_string().contains("outside the proven witness"),
        "unexpected CREATE witness error: {error}"
    );
    assert!(dtvm
        .last_audit()
        .iter()
        .any(|event| matches!(event, AccessEvent::StorageWrite(..))));
    assert!(dtvm
        .last_audit()
        .iter()
        .any(|event| matches!(event, AccessEvent::NestedCall(3))));
    let journal = &dtvm.all().0.journaled_state.inner;
    assert!(journal.state.is_empty());
    assert!(journal.journal.is_empty());
    assert!(journal.logs.is_empty());
}

#[test]
fn low_non_precompile_contract_reaches_dtvm_and_matches_reth() {
    let library = verified_dtvm_library();
    let fixture = ordinary_fixture_at(LOW_REGULAR_CONTRACT, LOW_ADDRESS_RETURN);
    let env = osaka_env();
    let tx = TxEnv {
        tx_type: 0,
        caller: SENDER,
        gas_limit: 100_000,
        gas_price: 1,
        kind: TxKind::Call(LOW_REGULAR_CONTRACT),
        nonce: 0,
        chain_id: Some(1),
        ..Default::default()
    };

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference.transact_raw(tx.clone()).unwrap();
    let reference_accesses = reference.db().accesses().to_vec();
    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm.transact_raw(tx).unwrap();

    assert_eq!(dtvm_outcome.result, reference_outcome.result);
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_accesses);
    assert!(dtvm
        .last_audit()
        .contains(&AccessEvent::AccountExists(to_dtvm_address(
            LOW_REGULAR_CONTRACT
        ))));
}

#[test]
fn missing_storage_fails_closed_in_both_backends() {
    let library = verified_dtvm_library();
    let fixture = strict_fixture(false, STORAGE_LOG_RETURN);
    let env = osaka_env();
    let tx = supported_tx();

    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_error = reference
        .transact_raw(tx.clone())
        .expect_err("REVM must reject missing storage witness");
    assert!(matches!(
        reference_error,
        EVMError::Database(StrictDbError::MissingStorage(RECIPIENT, key))
            if key == U256::ZERO
    ));

    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_error = dtvm
        .transact_raw(tx)
        .expect_err("DTVM host must reject missing storage witness");
    assert!(matches!(
        dtvm_error,
        EVMError::Database(StrictDbError::MissingStorage(RECIPIENT, key))
            if key == U256::ZERO
    ));
    assert!(
        !dtvm
            .db()
            .accesses()
            .iter()
            .any(|event| matches!(event, DbAccess::Basic(BENEFICIARY))),
        "execution continued into post-execution after a missing witness"
    );
}

#[test]
fn missing_nested_account_code_and_storage_witnesses_fail_closed_and_clear_journal() {
    let root_code = call_then_return(CHILD, 0);
    let child_code_hash = keccak256(STORAGE_LOG_RETURN);

    let missing_account = strict_fixture(false, &root_code);

    let mut missing_code = strict_fixture(false, &root_code);
    missing_code
        .insert_account(
            CHILD,
            AccountInfo {
                nonce: 1,
                code_hash: child_code_hash,
                account_id: Some(AccountId::new(8).unwrap()),
                code: None,
                ..Default::default()
            },
        )
        .unwrap();

    let mut missing_storage = strict_fixture(false, &root_code);
    insert_contract(
        &mut missing_storage,
        CHILD,
        8,
        STORAGE_LOG_RETURN,
        U256::ZERO,
    );

    for (fixture, expected) in [
        (missing_account, StrictDbError::MissingAccount(CHILD)),
        (missing_code, StrictDbError::MissingCode(child_code_hash)),
        (
            missing_storage,
            StrictDbError::MissingStorage(CHILD, U256::ZERO),
        ),
    ] {
        let env = osaka_env();
        let tx = supported_tx();
        let mut reference = EthEvmFactory::default()
            .create_evm(fixture.clone(), env.clone())
            .into_inner();
        let reference_error = ExecuteEvm::transact_one(&mut reference, tx.clone())
            .expect_err("stock Reth must reject an incomplete child witness");
        match reference_error {
            EVMError::Database(error) => assert_eq!(error, expected),
            other => panic!("unexpected stock Reth error: {other}"),
        }
        let reference_accesses = reference
            .all()
            .0
            .journaled_state
            .database
            .accesses()
            .to_vec();

        let mut dtvm = DtvmEvmFactory::new(verified_dtvm_library()).create_evm(fixture, env);
        let dtvm_error = ExecuteEvm::transact_one(&mut dtvm, tx)
            .expect_err("DTVM recursion must reject an incomplete child witness");
        match dtvm_error {
            EVMError::Database(error) => assert_eq!(error, expected),
            other => panic!("unexpected DTVM error: {other}"),
        }
        assert!(
            reference_accesses.starts_with(dtvm.db().accesses()),
            "DTVM must make the same witness reads through the first fatal child access"
        );
        assert!(!dtvm
            .db()
            .accesses()
            .iter()
            .any(|access| matches!(access, DbAccess::Basic(address) if *address == BENEFICIARY)));
        let journal = &dtvm.all().0.journaled_state.inner;
        assert!(journal.journal.is_empty());
        assert!(journal.logs.is_empty());
        for account in journal.state.values() {
            for slot in account.storage.values() {
                assert_eq!(slot.present_value, slot.original_value);
            }
        }
        let sender = journal.state.get(&SENDER).expect("sender remains loaded");
        assert_eq!(sender.info.nonce, 0);
        assert_eq!(sender.info.balance, U256::from(10_000_000u64));
    }
}

#[test]
fn missing_account_code_and_post_frame_faults_fail_closed() {
    let library = verified_dtvm_library();
    let env = osaka_env();
    let tx = supported_tx();

    let missing_recipient = strict_fixture_options(false, STORAGE_LOG_RETURN, false, true, true);
    let mut reference = EthEvmFactory::default().create_evm(missing_recipient.clone(), env.clone());
    assert!(matches!(
        reference.transact_raw(tx.clone()),
        Err(EVMError::Database(StrictDbError::MissingAccount(RECIPIENT)))
    ));
    let mut dtvm = DtvmEvmFactory::new(&library).create_evm(missing_recipient, env.clone());
    assert!(matches!(
        dtvm.transact_raw(tx.clone()),
        Err(EVMError::Database(StrictDbError::MissingAccount(RECIPIENT)))
    ));
    assert!(
        dtvm.last_audit().is_empty(),
        "missing recipient witness must fail before entering DTVM"
    );

    let code_hash = keccak256(STORAGE_LOG_RETURN);
    let missing_code = strict_fixture_options(true, STORAGE_LOG_RETURN, true, false, true);
    let mut reference = EthEvmFactory::default().create_evm(missing_code.clone(), env.clone());
    assert!(matches!(
        reference.transact_raw(tx.clone()),
        Err(EVMError::Database(StrictDbError::MissingCode(hash))) if hash == code_hash
    ));
    let mut dtvm = DtvmEvmFactory::new(&library).create_evm(missing_code, env.clone());
    assert!(matches!(
        dtvm.transact_raw(tx.clone()),
        Err(EVMError::Database(StrictDbError::MissingCode(hash))) if hash == code_hash
    ));
    assert!(
        dtvm.last_audit().is_empty(),
        "missing code witness must fail before entering DTVM"
    );

    let missing_beneficiary = strict_fixture_options(true, STORAGE_LOG_RETURN, true, true, false);
    let mut reference =
        EthEvmFactory::default().create_evm(missing_beneficiary.clone(), env.clone());
    assert!(matches!(
        reference.transact_raw(tx.clone()),
        Err(EVMError::Database(StrictDbError::MissingAccount(
            BENEFICIARY
        )))
    ));
    let mut dtvm = DtvmEvmFactory::new(library).create_evm(missing_beneficiary, env);
    assert!(matches!(
        ExecuteEvm::transact_one(&mut dtvm, tx),
        Err(EVMError::Database(StrictDbError::MissingAccount(
            BENEFICIARY
        )))
    ));
    assert_eq!(
        dtvm.last_audit(),
        expected_dtvm_audit(),
        "post-frame fault must happen after the exact DTVM host trace"
    );

    let journal = &dtvm.all().0.journaled_state.inner;
    assert!(
        journal.logs.is_empty(),
        "transaction checkpoint must remove the DTVM-emitted log"
    );
    assert!(
        journal.journal.is_empty(),
        "MainnetHandler::catch_error must discard the transaction journal"
    );
    let recipient = journal
        .state
        .get(&RECIPIENT)
        .expect("recipient remains loaded after transaction discard");
    let slot = recipient
        .storage
        .get(&U256::ZERO)
        .expect("slot remains loaded after transaction discard");
    assert_eq!(
        slot.present_value, slot.original_value,
        "transaction discard must remove the DTVM storage write"
    );
    let sender = journal
        .state
        .get(&SENDER)
        .expect("sender remains loaded after transaction discard");
    assert_eq!(sender.info.nonce, 0, "transaction discard restores nonce");
    assert_eq!(
        sender.info.balance,
        U256::from(10_000_000u64),
        "transaction discard restores sender balance"
    );
}

#[test]
fn type4_invalid_envelope_and_inspector_remain_owned_by_reth() {
    let library = verified_dtvm_library();
    let env = osaka_env();

    let mut invalid_type4 = type4_tx(RECIPIENT, Vec::new(), 0);
    invalid_type4.kind = TxKind::Create;
    let fixture = strict_fixture(true, STORAGE_LOG_RETURN);
    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_error = reference
        .transact_raw(invalid_type4.clone())
        .expect_err("stock Reth must reject an empty type-4 authorization list");
    let mut dtvm = DtvmEvmFactory::new(&library).create_evm(fixture, env.clone());
    let dtvm_error = dtvm
        .transact_raw(invalid_type4)
        .expect_err("DTVM shell must preserve stock type-4 validation");
    assert!(matches!(
        reference_error,
        EVMError::Transaction(InvalidTransaction::EmptyAuthorizationList)
    ));
    assert!(matches!(
        dtvm_error,
        EVMError::Transaction(InvalidTransaction::EmptyAuthorizationList)
    ));
    assert!(dtvm.last_audit().is_empty());

    let mut inspected = DtvmEvmFactory::new(&library).create_evm_with_inspector(
        strict_fixture(true, STORAGE_LOG_RETURN),
        env.clone(),
        AttemptInspector::default(),
    );
    let error = inspected
        .transact_raw(type4_tx(
            RECIPIENT,
            vec![recovered_authorization(1, CHILD, 0, Some(RECIPIENT))],
            0,
        ))
        .expect_err("inspected type-4 transaction must fail closed");
    assert!(error
        .to_string()
        .contains("inspector execution is fail-closed"));

    let mut inspected_system = DtvmEvmFactory::new(&library).create_evm_with_inspector(
        system_fixture(&HISTORY_STORAGE_RETURN_CODE, true, false),
        env,
        AttemptInspector::default(),
    );
    let error = inspected_system
        .transact_system_call(SYSTEM_ADDRESS, HISTORY_STORAGE_ADDRESS, Bytes::new())
        .expect_err("inspected system calls must fail closed");
    assert!(error
        .to_string()
        .contains("inspector execution is fail-closed"));
    assert!(inspected_system.db().accesses().is_empty());

    let mut failed_loader = DtvmEvmFactory::new("/definitely/missing/libdtvmapi.so")
        .create_evm(strict_fixture(true, STORAGE_LOG_RETURN), osaka_env());
    let error = failed_loader
        .transact_system_call(SYSTEM_ADDRESS, HISTORY_STORAGE_ADDRESS, Bytes::new())
        .expect_err("system call loader failure must surface before state access");
    assert!(error.to_string().contains("EVMC subject load failed"));
    assert!(failed_loader.db().accesses().is_empty());
    let error = failed_loader
        .transact_raw(supported_tx())
        .expect_err("factory load failure must surface at transaction time");
    assert!(error.to_string().contains("EVMC subject load failed"));
    assert!(
        failed_loader.db().accesses().is_empty(),
        "a failed loader must not touch state"
    );
}

fn append_ordinary_call(
    code: &mut Vec<u8>,
    target: Address,
    value: u8,
    input_offset: u8,
    input_size: u8,
    output_offset: u8,
    output_size: u8,
) {
    for value in [output_size, output_offset, input_size, input_offset, value] {
        code.extend_from_slice(&[opcode::PUSH1, value]);
    }
    code.push(opcode::PUSH20);
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[opcode::PUSH3, 0x07, 0xa1, 0x20, opcode::CALL]);
}

fn append_staticcall(
    code: &mut Vec<u8>,
    target: Address,
    input_offset: u8,
    input_size: u8,
    output_offset: u8,
    output_size: u8,
) {
    for value in [output_size, output_offset, input_size, input_offset] {
        code.extend_from_slice(&[opcode::PUSH1, value]);
    }
    code.push(opcode::PUSH20);
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[opcode::PUSH3, 0x07, 0xa1, 0x20, opcode::STATICCALL]);
}

fn append_delegatecall(
    code: &mut Vec<u8>,
    target: Address,
    input_offset: u8,
    input_size: u8,
    output_offset: u8,
    output_size: u8,
) {
    for value in [output_size, output_offset, input_size, input_offset] {
        code.extend_from_slice(&[opcode::PUSH1, value]);
    }
    code.push(opcode::PUSH20);
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[opcode::PUSH3, 0x07, 0xa1, 0x20, opcode::DELEGATECALL]);
}

fn append_callcode(
    code: &mut Vec<u8>,
    target: Address,
    value: u8,
    input_offset: u8,
    input_size: u8,
    output_offset: u8,
    output_size: u8,
) {
    append_ordinary_call(
        code,
        target,
        value,
        input_offset,
        input_size,
        output_offset,
        output_size,
    );
    *code.last_mut().expect("ordinary call opcode") = opcode::CALLCODE;
}

fn call_then_return(target: Address, output_size: u8) -> Vec<u8> {
    let mut code = Vec::new();
    append_ordinary_call(&mut code, target, 0, 0, 0, 0, output_size);
    code.push(opcode::POP);
    code.extend_from_slice(&[opcode::PUSH1, output_size, opcode::PUSH0, opcode::RETURN]);
    code
}

fn staticcall_then_return(target: Address, output_size: u8) -> Vec<u8> {
    let mut code = Vec::new();
    append_staticcall(&mut code, target, 0, 0, 0, output_size);
    code.push(opcode::POP);
    code.extend_from_slice(&[opcode::PUSH1, output_size, opcode::PUSH0, opcode::RETURN]);
    code
}

fn selfdestruct_code(beneficiary: Address) -> Vec<u8> {
    let mut code = vec![opcode::PUSH20];
    code.extend_from_slice(beneficiary.as_slice());
    code.push(opcode::SELFDESTRUCT);
    code
}

fn initcode_returning(runtime: &[u8]) -> Vec<u8> {
    let runtime_len = u8::try_from(runtime.len()).expect("test runtime fits PUSH1");
    let mut initcode = vec![
        opcode::PUSH1,
        runtime_len,
        opcode::PUSH1,
        0,
        opcode::PUSH0,
        opcode::CODECOPY,
        opcode::PUSH1,
        runtime_len,
        opcode::PUSH0,
        opcode::RETURN,
    ];
    initcode[3] = u8::try_from(initcode.len()).expect("test initcode offset fits PUSH1");
    initcode.extend_from_slice(runtime);
    initcode
}

fn create_then_return(initcode: &[u8], create2: bool, value: u8, salt: u8) -> Vec<u8> {
    let initcode_len = u8::try_from(initcode.len()).expect("test initcode fits PUSH1");
    let mut code = vec![
        opcode::PUSH1,
        initcode_len,
        opcode::PUSH1,
        0,
        opcode::PUSH0,
        opcode::CODECOPY,
    ];
    if create2 {
        code.extend_from_slice(&[opcode::PUSH1, salt]);
    }
    code.extend_from_slice(&[
        opcode::PUSH1,
        initcode_len,
        opcode::PUSH0,
        opcode::PUSH1,
        value,
        if create2 {
            opcode::CREATE2
        } else {
            opcode::CREATE
        },
        opcode::PUSH0,
        opcode::MSTORE,
        opcode::PUSH1,
        0x20,
        opcode::PUSH0,
        opcode::RETURN,
    ]);
    code[3] = u8::try_from(code.len()).expect("test root offset fits PUSH1");
    code.extend_from_slice(initcode);
    code
}

fn insert_contract(
    db: &mut StrictDb,
    address: Address,
    account_id: usize,
    code_bytes: &[u8],
    balance: U256,
) {
    let code = Bytecode::new_raw(Bytes::copy_from_slice(code_bytes));
    let code_hash = keccak256(code_bytes);
    db.insert_account(
        address,
        AccountInfo {
            balance,
            nonce: 1,
            code_hash,
            account_id: Some(AccountId::new(account_id).unwrap()),
            code: None,
        },
    )
    .unwrap();
    db.insert_code(code_hash, code).unwrap();
}

fn insert_delegation(db: &mut StrictDb, authority: Address, account_id: usize, delegate: Address) {
    let delegation = Bytecode::new_eip7702(delegate);
    let code_hash = keccak256(delegation.original_bytes());
    db.insert_account(
        authority,
        AccountInfo {
            nonce: 1,
            code_hash,
            account_id: Some(AccountId::new(account_id).unwrap()),
            code: None,
            ..Default::default()
        },
    )
    .unwrap();
    db.insert_code(code_hash, delegation).unwrap();
}

fn assert_nested_diff(
    root_code: &[u8],
    fixture: StrictDb,
) -> (ExecutionResult, EvmState, Vec<AccessEvent>) {
    assert_nested_diff_with_tx(root_code, fixture, supported_tx())
}

fn assert_nested_diff_with_tx(
    root_code: &[u8],
    fixture: StrictDb,
    tx: TxEnv,
) -> (ExecutionResult, EvmState, Vec<AccessEvent>) {
    let library = verified_dtvm_library();
    let env = osaka_env();
    let mut reference = EthEvmFactory::default().create_evm(fixture.clone(), env.clone());
    let reference_outcome = reference.transact_raw(tx.clone()).unwrap();
    let reference_accesses = reference.db().accesses().to_vec();
    let mut dtvm = DtvmEvmFactory::new(library).create_evm(fixture, env);
    let dtvm_outcome = dtvm.transact_raw(tx).unwrap();
    assert_eq!(
        dtvm_outcome.result,
        reference_outcome.result,
        "nested result mismatch for root code {}",
        encode_hex(root_code)
    );
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm.db().accesses(), reference_accesses);
    (
        dtvm_outcome.result,
        dtvm_outcome.state,
        dtvm.last_audit().to_vec(),
    )
}

fn strict_fixture(cover_slot: bool, code_bytes: &[u8]) -> StrictDb {
    strict_fixture_options(cover_slot, code_bytes, true, true, true)
}

fn strict_fixture_options(
    cover_slot: bool,
    code_bytes: &[u8],
    insert_recipient: bool,
    insert_code: bool,
    cover_beneficiary: bool,
) -> StrictDb {
    let mut db = StrictDb::default();
    db.insert_account(
        SENDER,
        AccountInfo {
            balance: U256::from(10_000_000u64),
            code: None,
            ..Default::default()
        },
    )
    .unwrap();

    let code = Bytecode::new_raw(Bytes::copy_from_slice(code_bytes));
    let code_hash = keccak256(code_bytes);
    if insert_recipient {
        db.insert_account(
            RECIPIENT,
            AccountInfo {
                nonce: 1,
                code_hash,
                account_id: Some(AccountId::new(7).unwrap()),
                code: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    if insert_code {
        db.insert_code(code_hash, code).unwrap();
    }
    if cover_slot {
        db.cover_storage(RECIPIENT, U256::ZERO, U256::ZERO).unwrap();
    }
    if cover_beneficiary {
        db.cover_absent_account(BENEFICIARY);
    }
    db
}

fn empty_target_fixture(target: Address) -> StrictDb {
    let mut db = StrictDb::default();
    db.insert_account(
        SENDER,
        AccountInfo {
            balance: U256::from(10_000_000u64),
            code: None,
            ..Default::default()
        },
    )
    .unwrap();
    db.cover_absent_account(target);
    db.cover_absent_account(BENEFICIARY);
    db
}

fn ordinary_fixture_at(target: Address, code_bytes: &[u8]) -> StrictDb {
    let mut db = StrictDb::default();
    db.insert_account(
        SENDER,
        AccountInfo {
            balance: U256::from(10_000_000u64),
            code: None,
            ..Default::default()
        },
    )
    .unwrap();
    let code = Bytecode::new_raw(Bytes::copy_from_slice(code_bytes));
    let code_hash = keccak256(code_bytes);
    db.insert_account(
        target,
        AccountInfo {
            code_hash,
            account_id: Some(AccountId::new(17).unwrap()),
            code: None,
            ..Default::default()
        },
    )
    .unwrap();
    db.insert_code(code_hash, code).unwrap();
    db.cover_absent_account(BENEFICIARY);
    db
}

fn system_fixture(code_bytes: &[u8], cover_slot: bool, cover_nested_target: bool) -> StrictDb {
    let mut db = StrictDb::default();
    let code = Bytecode::new_raw(Bytes::copy_from_slice(code_bytes));
    let code_hash = keccak256(code_bytes);
    db.insert_account(
        HISTORY_STORAGE_ADDRESS,
        AccountInfo {
            code_hash,
            account_id: Some(AccountId::new(19).unwrap()),
            code: None,
            ..Default::default()
        },
    )
    .unwrap();
    db.insert_code(code_hash, code).unwrap();
    if cover_slot {
        db.cover_storage(HISTORY_STORAGE_ADDRESS, U256::ZERO, U256::ZERO)
            .unwrap();
    }
    if cover_nested_target {
        db.cover_absent_account(Address::with_last_byte(4));
    }
    // The production system caller may be absent and must not be loaded,
    // deducted, or validated as a code participant.
    db.cover_absent_account(SYSTEM_ADDRESS);
    db
}

fn osaka_env() -> EvmEnv {
    EvmEnv {
        block_env: BlockEnv {
            number: U256::from(24_000_000u64),
            beneficiary: BENEFICIARY,
            timestamp: U256::from(1_800_000_000u64),
            gas_limit: 30_000_000,
            basefee: 0,
            ..Default::default()
        },
        cfg_env: CfgEnv::new_with_spec(SpecId::OSAKA).with_chain_id(1),
    }
}

fn supported_tx() -> TxEnv {
    TxEnv {
        tx_type: 0,
        caller: SENDER,
        gas_limit: SUPPORTED_TX_GAS_LIMIT,
        gas_price: 1,
        kind: TxKind::Call(RECIPIENT),
        nonce: 0,
        chain_id: Some(1),
        ..Default::default()
    }
}

fn top_level_create_tx(tx_type: u8, initcode: &[u8], value: u64) -> TxEnv {
    let mut tx = TxEnv {
        tx_type,
        caller: SENDER,
        gas_limit: 300_000,
        gas_price: 1,
        gas_priority_fee: (tx_type == 2).then_some(1),
        kind: TxKind::Create,
        value: U256::from(value),
        data: Bytes::copy_from_slice(initcode),
        nonce: 0,
        chain_id: Some(1),
        ..Default::default()
    };
    if tx_type == 3 {
        let mut versioned_hash = [0x55; 32];
        versioned_hash[0] = 0x01;
        tx.blob_hashes = vec![B256::from(versioned_hash)];
        tx.max_fee_per_blob_gas = 1;
    }
    tx
}

fn recovered_authorization(
    chain_id: u64,
    address: Address,
    nonce: u64,
    authority: Option<Address>,
) -> RecoveredAuthorization {
    RecoveredAuthorization::new_unchecked(
        Authorization {
            chain_id: U256::from(chain_id),
            address,
            nonce,
        },
        authority.map_or(RecoveredAuthority::Invalid, RecoveredAuthority::Valid),
    )
}

fn type4_tx(target: Address, authorizations: Vec<RecoveredAuthorization>, value: u64) -> TxEnv {
    let mut tx = TxEnv {
        tx_type: 4,
        caller: SENDER,
        gas_limit: 300_000,
        gas_price: 1,
        gas_priority_fee: Some(1),
        kind: TxKind::Call(target),
        value: U256::from(value),
        nonce: 0,
        chain_id: Some(1),
        ..Default::default()
    };
    tx.set_recovered_authorization(authorizations);
    tx
}

fn authority_fixture(authority: Address, nonce: u64, code: Option<Bytecode>) -> StrictDb {
    let mut db = StrictDb::default();
    db.insert_account(
        SENDER,
        AccountInfo {
            balance: U256::from(10_000_000u64),
            ..Default::default()
        },
    )
    .unwrap();
    let code_hash = code
        .as_ref()
        .map(|code| keccak256(code.original_bytes()))
        .unwrap_or_else(|| keccak256([]));
    db.insert_account(
        authority,
        AccountInfo {
            nonce,
            code_hash,
            account_id: Some(AccountId::new(7).unwrap()),
            code: None,
            ..Default::default()
        },
    )
    .unwrap();
    if let Some(code) = code {
        db.insert_code(code_hash, code).unwrap();
    }
    db.cover_absent_account(BENEFICIARY);
    db
}

fn logical_dtvm_attempts(audit: &[AccessEvent]) -> Vec<LogicalAttempt> {
    audit
        .iter()
        .filter_map(|event| match event {
            AccessEvent::StorageWrite(address, key, _) => Some(LogicalAttempt::StorageWrite(
                to_revm_address(*address),
                to_u256(*key),
            )),
            AccessEvent::Log(address, ..) => Some(LogicalAttempt::Log(to_revm_address(*address))),
            AccessEvent::StorageRead(address, key) => Some(LogicalAttempt::StorageRead(
                to_revm_address(*address),
                to_u256(*key),
            )),
            _ => None,
        })
        .collect()
}

fn expected_dtvm_audit() -> Vec<AccessEvent> {
    let recipient = to_dtvm_address(RECIPIENT);
    let zero = Word::ZERO;
    let value = Word::from_u64(42);
    vec![
        AccessEvent::AccountExists(recipient),
        AccessEvent::AccountExists(recipient),
        AccessEvent::CodeSize(recipient),
        AccessEvent::CodeHash(recipient),
        AccessEvent::CodeCopy(recipient, 0, STORAGE_LOG_RETURN.len()),
        AccessEvent::StorageWarm(recipient, zero),
        AccessEvent::StorageWrite(recipient, zero, value),
        AccessEvent::Log(recipient, 32, 1),
        AccessEvent::StorageWarm(recipient, zero),
        AccessEvent::StorageRead(recipient, zero),
    ]
}

fn assert_state_semantics_eq(actual: &EvmState, expected: &EvmState) {
    let mut actual_addresses = actual.keys().copied().collect::<Vec<_>>();
    let mut expected_addresses = expected.keys().copied().collect::<Vec<_>>();
    actual_addresses.sort();
    expected_addresses.sort();
    assert_eq!(actual_addresses, expected_addresses, "state address set");

    for address in actual_addresses {
        let actual = &actual[&address];
        let expected = &expected[&address];
        assert_eq!(
            actual.info.balance, expected.info.balance,
            "{address} balance"
        );
        assert_eq!(actual.info.nonce, expected.info.nonce, "{address} nonce");
        assert_eq!(
            actual.info.code_hash, expected.info.code_hash,
            "{address} code hash"
        );
        assert_eq!(
            actual.info.account_id, expected.info.account_id,
            "{address} account id hint"
        );
        assert_eq!(
            code_bytes(actual),
            code_bytes(expected),
            "{address} actual code bytes"
        );
        assert_eq!(actual.status, expected.status, "{address} account status");

        let mut actual_slots = actual.storage.keys().copied().collect::<Vec<_>>();
        let mut expected_slots = expected.storage.keys().copied().collect::<Vec<_>>();
        actual_slots.sort();
        expected_slots.sort();
        assert_eq!(actual_slots, expected_slots, "{address} storage key set");
        for key in actual_slots {
            assert_eq!(
                actual.storage[&key].original_value, expected.storage[&key].original_value,
                "{address}[{key}] original"
            );
            assert_eq!(
                actual.storage[&key].present_value, expected.storage[&key].present_value,
                "{address}[{key}] present"
            );
        }
    }
}

fn code_bytes(account: &Account) -> Option<Vec<u8>> {
    account
        .info
        .code
        .as_ref()
        .map(|code| code.original_bytes().to_vec())
}

fn to_revm_address(address: reth_dtvm_adapter::Address) -> Address {
    Address::from(address.0)
}

fn to_dtvm_address(address: Address) -> DtvmAddress {
    DtvmAddress(address.0 .0)
}

fn to_u256(word: Word) -> U256 {
    U256::from_be_bytes(word.0)
}

fn verified_dtvm_library() -> PathBuf {
    assert_eq!(
        std::env::var("DTVM_REQUIRED").as_deref(),
        Ok("1"),
        "real transaction tests require DTVM_REQUIRED=1"
    );
    let path = PathBuf::from(std::env::var("DTVM_LIBRARY").expect("DTVM_LIBRARY is mandatory"));
    let expected = std::env::var("DTVM_LIBRARY_SHA256").expect("DTVM_LIBRARY_SHA256 is mandatory");
    let mut file = File::open(&path).expect("open DTVM_LIBRARY");
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).expect("read DTVM_LIBRARY");
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(actual, expected, "DTVM library SHA-256 mismatch");
    path
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(2 + bytes.len() * 2);
    output.push_str("0x");
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}
