use alloy_evm::{eth::EthEvmFactory, Evm, EvmEnv, EvmFactory};
use alloy_primitives::{Address as AlloyAddress, Bytes, TxKind, U256};
use reth_dtvm_adapter::{
    host::AccessEvent, Account, Address, Dtvm, FrameStatus, HostBackend, HostContext, Message,
    TxContextOwned, WitnessBackend, Word,
};
use revm::{
    bytecode::Bytecode,
    context::TxEnv,
    context_interface::result::ExecutionResult,
    database::{CacheDB, EmptyDB},
    primitives::hardfork::SpecId,
    state::AccountInfo,
};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read};

const EVMC_OSAKA: i32 = 14;

fn dtvm_library() -> String {
    assert_eq!(
        std::env::var("DTVM_REQUIRED").as_deref(),
        Ok("1"),
        "real DTVM tests require DTVM_REQUIRED=1"
    );
    let path = std::env::var("DTVM_LIBRARY").expect("DTVM_LIBRARY is mandatory");
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

/// PUSH1 0x2a PUSH0 SSTORE
/// PUSH1 0x2a PUSH0 MSTORE
/// PUSH1 0x01 PUSH1 0x20 PUSH0 LOG1
/// PUSH0 SLOAD PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
const STORAGE_LOG_RETURN: &[u8] = &[
    0x60, 0x2a, 0x5f, 0x55, 0x60, 0x2a, 0x5f, 0x52, 0x60, 0x01, 0x60, 0x20, 0x5f, 0xa1, 0x5f, 0x54,
    0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
];

#[test]
fn real_dtvm_storage_log_frame_is_fail_closed_and_commits() {
    let library = dtvm_library();

    let recipient = Address([0x11; 20]);
    let sender = Address([0x22; 20]);
    let key = Word::ZERO;
    let value = Word::from_u64(0x2a);
    let mut backend = WitnessBackend::default();
    backend.insert_account(
        recipient,
        Account::new(Word::ZERO, STORAGE_LOG_RETURN.to_vec()),
    );
    backend.insert_account(sender, Account::new(Word::from_u64(1_000_000), Vec::new()));
    backend.cover_storage(recipient, key, Word::ZERO).unwrap();
    backend.prewarm_account(recipient).unwrap();
    backend.prewarm_account(sender).unwrap();

    let mut host = HostContext::new(&mut backend, TxContextOwned::default());
    let message = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 1_000_000);
    // SAFETY: the test path is supplied by the isolated experiment and pinned
    // in provenance before execution.
    let mut vm = unsafe { Dtvm::load(library) }.unwrap();
    let outcome = vm
        .execute(EVMC_OSAKA, &message, STORAGE_LOG_RETURN, &mut host)
        .unwrap();

    assert_eq!(outcome.status, FrameStatus::Success);
    assert_eq!(outcome.output, value.0);
    assert!(outcome.gas_left < message.gas);
    assert!(outcome.audit.iter().any(|event| {
        matches!(event, AccessEvent::StorageWarm(address, slot) if *address == recipient && *slot == key)
    }));
    assert!(outcome.audit.iter().any(|event| {
        matches!(event, AccessEvent::StorageWrite(address, slot, new) if *address == recipient && *slot == key && *new == value)
    }));
    assert!(outcome.audit.iter().any(|event| {
        matches!(event, AccessEvent::Log(address, 32, 1) if *address == recipient)
    }));
    drop(host);

    assert_eq!(
        backend.account(recipient).unwrap().present_storage(key),
        Some(value)
    );
    let logs = backend.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].address, recipient);
    assert_eq!(logs[0].data, value.0);
    assert_eq!(logs[0].topics, vec![Word::from_u64(1)]);
}

#[test]
fn real_dtvm_matches_revm_for_storage_log_return_vm_slice() {
    let library = dtvm_library();

    const TX_GAS: u64 = 1_000_000;
    const INTRINSIC_GAS: u64 = 21_000;
    let recipient = Address([0x11; 20]);
    let sender = Address([0x22; 20]);
    let recipient_revm = AlloyAddress::from(recipient.0);
    let sender_revm = AlloyAddress::from(sender.0);
    let key = Word::ZERO;
    let value = Word::from_u64(0x2a);

    let mut witness = WitnessBackend::default();
    witness.insert_account(
        recipient,
        Account::new(Word::ZERO, STORAGE_LOG_RETURN.to_vec()),
    );
    witness.insert_account(sender, Account::new(Word::from_u64(10_000_000), Vec::new()));
    witness.cover_storage(recipient, key, Word::ZERO).unwrap();
    witness.prewarm_account(recipient).unwrap();
    witness.prewarm_account(sender).unwrap();
    let mut host = HostContext::new(&mut witness, TxContextOwned::default());
    let message = Message::ordinary(
        recipient,
        sender,
        Vec::new(),
        Word::ZERO,
        TX_GAS - INTRINSIC_GAS,
    );
    // SAFETY: the test path is supplied by the isolated experiment and pinned
    // in provenance before execution.
    let mut dtvm = unsafe { Dtvm::load(library) }.unwrap();
    let dtvm_outcome = dtvm
        .execute(EVMC_OSAKA, &message, STORAGE_LOG_RETURN, &mut host)
        .unwrap();
    drop(host);

    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(
        sender_revm,
        AccountInfo {
            balance: U256::from(10_000_000u64),
            ..Default::default()
        },
    );
    db.insert_account_info(
        recipient_revm,
        AccountInfo::default().with_code(Bytecode::new_raw(Bytes::copy_from_slice(
            STORAGE_LOG_RETURN,
        ))),
    );
    db.insert_account_storage(recipient_revm, U256::ZERO, U256::ZERO)
        .unwrap();
    let mut env = EvmEnv::default();
    env.cfg_env.spec = SpecId::OSAKA;
    env.cfg_env.chain_id = 1;
    let mut revm = EthEvmFactory::default().create_evm(db, env);
    let revm_outcome = revm
        .transact_raw(TxEnv {
            caller: sender_revm,
            gas_limit: TX_GAS,
            kind: TxKind::Call(recipient_revm),
            chain_id: Some(1),
            ..Default::default()
        })
        .unwrap();

    let ExecutionResult::Success {
        gas, logs, output, ..
    } = &revm_outcome.result
    else {
        panic!("REVM did not succeed: {:?}", revm_outcome.result);
    };
    assert_eq!(dtvm_outcome.status, FrameStatus::Success);
    assert_eq!(dtvm_outcome.output.as_slice(), output.data().as_ref());
    assert_eq!(
        (message.gas - dtvm_outcome.gas_left) + INTRINSIC_GAS,
        gas.total_gas_spent()
    );
    assert_eq!(
        dtvm_outcome.gas_refund,
        i64::try_from(gas.inner_refunded()).expect("REVM refund fits EVMC i64")
    );

    let dtvm_logs = witness.logs();
    assert_eq!(dtvm_logs.len(), logs.len());
    assert_eq!(logs[0].address.as_slice(), dtvm_logs[0].address.0);
    assert_eq!(logs[0].data.data.as_ref(), dtvm_logs[0].data.as_slice());
    assert_eq!(logs[0].data.topics().len(), dtvm_logs[0].topics.len());
    assert_eq!(
        logs[0].data.topics()[0].as_slice(),
        dtvm_logs[0].topics[0].0
    );

    let revm_slot = revm_outcome
        .state
        .get(&recipient_revm)
        .and_then(|account| account.storage.get(&U256::ZERO))
        .expect("REVM changed storage slot");
    assert_eq!(revm_slot.present_value(), U256::from(0x2au64));
    assert_eq!(
        witness.account(recipient).unwrap().present_storage(key),
        Some(value)
    );

    let storage_warm_count = dtvm_outcome
        .audit
        .iter()
        .filter(|event| matches!(event, AccessEvent::StorageWarm(_, _)))
        .count();
    let storage_read_count = dtvm_outcome
        .audit
        .iter()
        .filter(|event| matches!(event, AccessEvent::StorageRead(_, _)))
        .count();
    let storage_write_count = dtvm_outcome
        .audit
        .iter()
        .filter(|event| matches!(event, AccessEvent::StorageWrite(_, _, _)))
        .count();
    assert_eq!(
        (storage_warm_count, storage_read_count, storage_write_count),
        (2, 1, 1)
    );
    println!(
        concat!(
            "DTVM_REVM_DIFF_JSON={{",
            "\"dtvm_status\":\"success\",",
            "\"revm_status\":\"success\",",
            "\"dtvm_frame_gas_limit\":{},",
            "\"dtvm_gas_left\":{},",
            "\"dtvm_frame_gas_used\":{},",
            "\"dtvm_refund\":{},",
            "\"revm_total_gas_spent\":{},",
            "\"revm_intrinsic_gas\":{},",
            "\"revm_frame_gas_spent\":{},",
            "\"output_last_byte\":{},",
            "\"slot_value\":{},",
            "\"log_count\":{},",
            "\"log_data_last_byte\":{},",
            "\"log_topic_last_byte\":{},",
            "\"dtvm_access_event_count\":{},",
            "\"dtvm_storage_warm_events\":{},",
            "\"dtvm_storage_read_events\":{},",
            "\"dtvm_storage_write_events\":{}",
            "}}"
        ),
        message.gas,
        dtvm_outcome.gas_left,
        message.gas - dtvm_outcome.gas_left,
        dtvm_outcome.gas_refund,
        gas.total_gas_spent(),
        INTRINSIC_GAS,
        gas.total_gas_spent() - INTRINSIC_GAS,
        dtvm_outcome.output[31],
        0x2a,
        dtvm_logs.len(),
        dtvm_logs[0].data[31],
        dtvm_logs[0].topics[0].0[31],
        dtvm_outcome.audit.len(),
        storage_warm_count,
        storage_read_count,
        storage_write_count,
    );
}

#[test]
fn real_dtvm_rejects_authoritative_code_mismatch_before_execution() {
    let library = dtvm_library();
    let recipient = Address([0x11; 20]);
    let sender = Address([0x22; 20]);
    let key = Word::ZERO;
    let mut backend = WitnessBackend::default();
    backend.insert_account(recipient, Account::new(Word::ZERO, vec![0x00]));
    backend.insert_account(sender, Account::new(Word::from_u64(1), Vec::new()));
    backend
        .cover_storage(recipient, key, Word::from_u64(7))
        .unwrap();
    backend.prewarm_account(recipient).unwrap();
    backend.prewarm_account(sender).unwrap();
    let mut host = HostContext::new(&mut backend, TxContextOwned::default());
    let message = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 100_000);
    // SAFETY: required verifier pins the shared object hash.
    let mut dtvm = unsafe { Dtvm::load(library) }.unwrap();
    let error = dtvm
        .execute(EVMC_OSAKA, &message, &[0x60, 0x00], &mut host)
        .unwrap_err();
    assert!(matches!(
        error,
        reth_dtvm_adapter::DtvmError::Host(
            reth_dtvm_adapter::HostFault::CodeMismatch(address)
        ) if address == recipient
    ));
    drop(host);
    assert_eq!(
        backend.account(recipient).unwrap().present_storage(key),
        Some(Word::from_u64(7))
    );
}

#[test]
fn real_dtvm_nested_call_is_sticky_fatal_and_rolls_back() {
    let library = dtvm_library();
    let recipient = Address([0x11; 20]);
    let sender = Address([0x22; 20]);
    let target = Address([0x33; 20]);
    let mut call_code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x5f, 0x73];
    call_code.extend_from_slice(&target.0);
    call_code.extend_from_slice(&[0x61, 0x03, 0xe8, 0xf1, 0x00]);

    let mut backend = WitnessBackend::default();
    backend.insert_account(recipient, Account::new(Word::ZERO, call_code.clone()));
    backend.insert_account(sender, Account::new(Word::from_u64(1), Vec::new()));
    backend.insert_account(target, Account::new(Word::ZERO, vec![0x00]));
    backend.prewarm_account(recipient).unwrap();
    backend.prewarm_account(sender).unwrap();
    let mut host = HostContext::new(&mut backend, TxContextOwned::default());
    let message = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 100_000);
    // SAFETY: required verifier pins the shared object hash.
    let mut dtvm = unsafe { Dtvm::load(library) }.unwrap();
    let error = dtvm
        .execute(EVMC_OSAKA, &message, &call_code, &mut host)
        .unwrap_err();
    assert!(matches!(
        error,
        reth_dtvm_adapter::DtvmError::Host(reth_dtvm_adapter::HostFault::NestedCallUnsupported {
            kind: 0
        })
    ));
}

#[test]
fn real_dtvm_selfdestruct_is_sticky_fatal() {
    let library = dtvm_library();
    let recipient = Address([0x11; 20]);
    let sender = Address([0x22; 20]);
    let beneficiary = Address([0x33; 20]);
    let mut code = vec![0x73];
    code.extend_from_slice(&beneficiary.0);
    code.push(0xff);

    let mut backend = WitnessBackend::default();
    backend.insert_account(recipient, Account::new(Word::from_u64(7), code.clone()));
    backend.insert_account(sender, Account::new(Word::from_u64(1), Vec::new()));
    backend.insert_account(beneficiary, Account::new(Word::ZERO, Vec::new()));
    backend.prewarm_account(recipient).unwrap();
    backend.prewarm_account(sender).unwrap();
    let mut host = HostContext::new(&mut backend, TxContextOwned::default());
    let message = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 100_000);
    // SAFETY: required verifier pins the shared object hash.
    let mut dtvm = unsafe { Dtvm::load(library) }.unwrap();
    let error = dtvm
        .execute(EVMC_OSAKA, &message, &code, &mut host)
        .unwrap_err();
    assert!(matches!(
        error,
        reth_dtvm_adapter::DtvmError::Host(reth_dtvm_adapter::HostFault::SelfdestructUnsupported)
    ));
}

#[test]
fn real_dtvm_missing_top_level_account_is_fatal_before_vm() {
    let library = dtvm_library();
    let recipient = Address([0x11; 20]);
    let sender = Address([0x22; 20]);
    let mut backend = WitnessBackend::default();
    backend.insert_account(sender, Account::new(Word::from_u64(1), Vec::new()));
    backend.prewarm_account(sender).unwrap();
    let mut host = HostContext::new(&mut backend, TxContextOwned::default());
    let message = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 100_000);
    // SAFETY: required verifier pins the shared object hash.
    let mut dtvm = unsafe { Dtvm::load(library) }.unwrap();
    let error = dtvm
        .execute(EVMC_OSAKA, &message, &[0x00], &mut host)
        .unwrap_err();
    assert!(matches!(
        error,
        reth_dtvm_adapter::DtvmError::Host(
            reth_dtvm_adapter::HostFault::MissingAccount(address)
        ) if address == recipient
    ));
}

#[test]
fn real_dtvm_low_non_precompile_address_is_not_rejected_by_prefix() {
    let library = dtvm_library();
    let mut raw_recipient = [0u8; 20];
    raw_recipient[18] = 1;
    let recipient = Address(raw_recipient);
    let sender = Address([0x22; 20]);
    let mut backend = WitnessBackend::default();
    backend.insert_account(recipient, Account::new(Word::ZERO, vec![0x00]));
    backend.prewarm_account(recipient).unwrap();
    let mut host = HostContext::new(&mut backend, TxContextOwned::default());
    let message = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 100_000);
    // SAFETY: required verifier pins the shared object hash.
    let mut dtvm = unsafe { Dtvm::load(library) }.unwrap();
    let outcome = dtvm
        .execute(EVMC_OSAKA, &message, &[0x00], &mut host)
        .unwrap();
    assert_eq!(outcome.status, FrameStatus::Success);
    assert!(
        !outcome.audit.contains(&AccessEvent::AccountExists(sender)),
        "caller validation belongs to the Reth shell, not the VM boundary"
    );
}

#[test]
fn real_dtvm_missing_storage_slot_is_sticky_fatal() {
    let library = dtvm_library();
    const SLOAD_STOP: &[u8] = &[0x5f, 0x54, 0x00];
    let recipient = Address([0x11; 20]);
    let sender = Address([0x22; 20]);
    let mut backend = WitnessBackend::default();
    backend.insert_account(recipient, Account::new(Word::ZERO, SLOAD_STOP.to_vec()));
    backend.insert_account(sender, Account::new(Word::from_u64(1), Vec::new()));
    backend.prewarm_account(recipient).unwrap();
    backend.prewarm_account(sender).unwrap();
    let mut host = HostContext::new(&mut backend, TxContextOwned::default());
    let message = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 100_000);
    // SAFETY: required verifier pins the shared object hash.
    let mut dtvm = unsafe { Dtvm::load(library) }.unwrap();
    let error = dtvm
        .execute(EVMC_OSAKA, &message, SLOAD_STOP, &mut host)
        .unwrap_err();
    assert!(matches!(
        error,
        reth_dtvm_adapter::DtvmError::Host(
            reth_dtvm_adapter::HostFault::MissingStorage(address, key)
        ) if address == recipient && key == Word::ZERO
    ));
}

#[test]
fn real_dtvm_rolls_back_storage_log_transient_and_warmth_before_nested_fault() {
    let library = dtvm_library();
    let recipient = Address([0x11; 20]);
    let sender = Address([0x22; 20]);
    let target = Address([0x33; 20]);
    let key = Word::ZERO;

    let mut code = vec![
        0x60, 0x2a, 0x5f, 0x55, // SSTORE(0, 42)
        0x60, 0x2a, 0x5f, 0x52, // MSTORE(0, 42)
        0x60, 0x01, 0x60, 0x20, 0x5f, 0xa1, // LOG1(topic=1, mem[0..32])
        0x60, 0x07, 0x5f, 0x5d, // TSTORE(0, 7)
        0x5f, 0x5f, 0x5f, 0x5f, 0x5f, 0x73, // CALL fixed arguments + PUSH20
    ];
    code.extend_from_slice(&target.0);
    code.extend_from_slice(&[0x61, 0x03, 0xe8, 0xf1, 0x00]);

    let mut backend = WitnessBackend::default();
    backend.insert_account(recipient, Account::new(Word::ZERO, code.clone()));
    backend.insert_account(sender, Account::new(Word::from_u64(1), Vec::new()));
    backend.insert_account(target, Account::new(Word::ZERO, vec![0x00]));
    backend.cover_storage(recipient, key, Word::ZERO).unwrap();
    backend.prewarm_account(recipient).unwrap();
    backend.prewarm_account(sender).unwrap();
    let mut host = HostContext::new(&mut backend, TxContextOwned::default());
    let message = Message::ordinary(recipient, sender, Vec::new(), Word::ZERO, 200_000);
    // SAFETY: required verifier pins the shared object hash.
    let mut dtvm = unsafe { Dtvm::load(library) }.unwrap();
    let error = dtvm
        .execute(EVMC_OSAKA, &message, &code, &mut host)
        .unwrap_err();
    assert!(matches!(
        error,
        reth_dtvm_adapter::DtvmError::Host(reth_dtvm_adapter::HostFault::NestedCallUnsupported {
            kind: 0
        })
    ));
    drop(host);

    assert_eq!(
        backend.account(recipient).unwrap().present_storage(key),
        Some(Word::ZERO)
    );
    assert!(backend.logs().is_empty());
    assert_eq!(
        HostBackend::get_transient_storage(&mut backend, recipient, key),
        Ok(Word::ZERO)
    );
    assert_eq!(
        HostBackend::access_storage(&mut backend, recipient, key),
        Ok(false),
        "storage warmth introduced before the nested fault must roll back"
    );
    assert_eq!(
        HostBackend::access_account(&mut backend, target),
        Ok(false),
        "dynamic target warmth introduced before the nested fault must roll back"
    );
}
