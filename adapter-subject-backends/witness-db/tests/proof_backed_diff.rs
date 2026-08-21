use alloy_consensus::{Header, TrieAccount};
use alloy_evm::{
    eth::{EthEvmContext, EthEvmFactory},
    Evm, EvmFactory,
};
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_trie::{TrieMask, EMPTY_ROOT_HASH, KECCAK_EMPTY};
use reth_dtvm_adapter::{AccessEvent, Address as DtvmAddress, Word};
use reth_dtvm_transaction_adapter::{
    DbAccess, DtvmEvmFactory, STORAGE_LOG_RETURN, SUPPORTED_TX_GAS_LIMIT,
};
use reth_dtvm_witness_db::{AccessManifest, WitnessBundle, WitnessDb};
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::EthEvmConfig;
use reth_trie_common::{BranchNodeV2, LeafNode, Nibbles, RlpNode, TrieNodeV2};
use revm::{
    bytecode::opcode,
    context::TxEnv,
    inspector::Inspector,
    interpreter::{interpreter::EthInterpreter, interpreter_types::Jumps, Interpreter},
    primitives::hardfork::SpecId,
    state::{Account, EvmState},
};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::PathBuf};

const TARGET_NUMBER: u64 = 24_000_000;
const TARGET_TIMESTAMP: u64 = 1_800_000_000;

struct SyntheticFixture {
    bundle: WitnessBundle,
    target: Header,
    sender: Address,
    recipient: Address,
    beneficiary: Address,
    pre_state_root: B256,
}

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

impl Inspector<EthEvmContext<WitnessDb>> for AttemptInspector {
    fn step(
        &mut self,
        interpreter: &mut Interpreter<EthInterpreter>,
        _context: &mut EthEvmContext<WitnessDb>,
    ) {
        let address = interpreter.input.target_address;
        match interpreter.bytecode.opcode() {
            opcode::SSTORE => self.attempts.push(LogicalAttempt::StorageWrite(
                address,
                interpreter.stack.peek(0).expect("synthetic SSTORE key"),
            )),
            opcode::SLOAD => self.attempts.push(LogicalAttempt::StorageRead(
                address,
                interpreter.stack.peek(0).expect("synthetic SLOAD key"),
            )),
            _ => {}
        }
    }

    fn log(&mut self, _context: &mut EthEvmContext<WitnessDb>, log: alloy_primitives::Log) {
        self.attempts.push(LogicalAttempt::Log(log.address));
    }
}

/// This is a synthetic, unsigned `TxEnv` differential for one ordinary CALL.
///
/// It proves that both factories can consume independent lazy `WitnessDb`
/// instances anchored to the same parent state root. It does not claim signed
/// transaction decoding, raw-block replay, or full consensus-header validation.
#[test]
fn proof_backed_synthetic_osaka_single_call_matches_stock_reth_and_real_dtvm() {
    let fixture = synthetic_fixture();
    assert_eq!(fixture.bundle.access_manifest, AccessManifest::default());

    let env = EthEvmConfig::mainnet()
        .evm_env(&fixture.target)
        .expect("derive mainnet EVM environment from synthetic target header");
    assert_eq!(env.cfg_env.spec, SpecId::OSAKA);
    assert_eq!(env.cfg_env.chain_id, 1);
    assert_eq!(env.block_env.number, U256::from(fixture.target.number));
    assert_eq!(
        env.block_env.timestamp,
        U256::from(fixture.target.timestamp)
    );
    assert_eq!(env.block_env.beneficiary, fixture.target.beneficiary);
    assert_eq!(env.block_env.gas_limit, fixture.target.gas_limit);
    assert_eq!(
        env.block_env.basefee,
        fixture.target.base_fee_per_gas.unwrap()
    );
    assert_eq!(env.block_env.prevrandao, Some(fixture.target.mix_hash));
    assert_eq!(fixture.target.block_access_list_hash, None);
    assert_eq!(fixture.target.slot_number, None);

    let mut reference_db =
        WitnessDb::from_bundle(fixture.bundle.clone()).expect("reference proof-backed database");
    let mut dtvm_db =
        WitnessDb::from_bundle(fixture.bundle.clone()).expect("DTVM proof-backed database");
    assert_eq!(reference_db.pre_state_root(), fixture.pre_state_root);
    assert_eq!(dtvm_db.pre_state_root(), fixture.pre_state_root);
    assert_eq!(reference_db.access_manifest(), &AccessManifest::default());
    assert_eq!(dtvm_db.access_manifest(), &AccessManifest::default());
    assert_eq!(
        reference_db.verified_root().unwrap(),
        fixture.pre_state_root
    );
    assert_eq!(dtvm_db.verified_root().unwrap(), fixture.pre_state_root);

    let tx = synthetic_tx(fixture.sender, fixture.recipient);
    let mut reference = EthEvmFactory::default().create_evm_with_inspector(
        reference_db,
        env.clone(),
        AttemptInspector::default(),
    );
    let reference_outcome = reference
        .transact_raw(tx.clone())
        .expect("stock REVM proof-backed synthetic transaction");
    let reference_attempts = reference.inspector().attempts.clone();
    let reference_accesses = reference.db().strict_db().accesses().to_vec();

    let mut dtvm = DtvmEvmFactory::new(verified_dtvm_library()).create_evm(dtvm_db, env);
    let dtvm_outcome = dtvm
        .transact_raw(tx)
        .expect("real DTVM proof-backed synthetic transaction");
    let dtvm_accesses = dtvm.db().strict_db().accesses().to_vec();
    let dtvm_audit = dtvm.last_audit().to_vec();

    assert_eq!(
        dtvm_outcome.result, reference_outcome.result,
        "complete ExecutionResult must match"
    );
    assert_state_semantics_eq(&dtvm_outcome.state, &reference_outcome.state);
    assert_eq!(dtvm_accesses, reference_accesses);
    assert_eq!(
        reference_accesses,
        [
            DbAccess::Basic(fixture.sender),
            DbAccess::Basic(fixture.recipient),
            DbAccess::Code(keccak256(STORAGE_LOG_RETURN)),
            DbAccess::Storage(fixture.recipient, U256::ZERO),
            DbAccess::Basic(fixture.beneficiary),
        ]
    );
    assert_eq!(logical_dtvm_attempts(&dtvm_audit), reference_attempts);
    assert_eq!(
        reference_attempts,
        [
            LogicalAttempt::StorageWrite(fixture.recipient, U256::ZERO),
            LogicalAttempt::Log(fixture.recipient),
            LogicalAttempt::StorageRead(fixture.recipient, U256::ZERO),
        ]
    );
}

fn synthetic_fixture() -> SyntheticFixture {
    let sender = Address::new([0x11; 20]);
    let sender_nibble = root_nibble(sender);
    let recipient = address_with_distinct_root_nibble(0x22, &[sender_nibble]);
    let recipient_nibble = root_nibble(recipient);
    let beneficiary = address_with_distinct_root_nibble(0x33, &[sender_nibble, recipient_nibble]);
    assert_ne!(root_nibble(beneficiary), sender_nibble);
    assert_ne!(root_nibble(beneficiary), recipient_nibble);

    let sender_account = TrieAccount {
        nonce: 0,
        balance: U256::from(10_000_000u64),
        storage_root: EMPTY_ROOT_HASH,
        code_hash: KECCAK_EMPTY,
    };
    let recipient_account = TrieAccount {
        nonce: 1,
        balance: U256::ZERO,
        storage_root: EMPTY_ROOT_HASH,
        code_hash: keccak256(STORAGE_LOG_RETURN),
    };
    let sender_node = account_leaf(sender, sender_account);
    let recipient_node = account_leaf(recipient, recipient_account);
    let state_root_node = branch_node(
        (sender_nibble, sender_node.as_slice()),
        (recipient_nibble, recipient_node.as_slice()),
    );
    let pre_state_root = keccak256(&state_root_node);

    let parent = synthetic_header(
        TARGET_NUMBER - 1,
        TARGET_TIMESTAMP - 12,
        Address::ZERO,
        pre_state_root,
        B256::repeat_byte(0x44),
    );
    let target = synthetic_header(
        TARGET_NUMBER,
        TARGET_TIMESTAMP,
        beneficiary,
        EMPTY_ROOT_HASH,
        parent.hash_slow(),
    );
    assert_eq!(parent.block_access_list_hash, None);
    assert_eq!(parent.slot_number, None);
    assert_eq!(target.block_access_list_hash, None);
    assert_eq!(target.slot_number, None);
    let bundle = WitnessBundle {
        target_header: alloy_rlp::encode(&target).into(),
        target_block_hash: target.hash_slow(),
        target_block: None,
        witness: ExecutionWitness {
            state: vec![
                state_root_node.into(),
                sender_node.into(),
                recipient_node.into(),
            ],
            codes: vec![Bytes::copy_from_slice(STORAGE_LOG_RETURN)],
            keys: vec![
                Bytes::copy_from_slice(sender.as_slice()),
                Bytes::copy_from_slice(recipient.as_slice()),
                Bytes::copy_from_slice(B256::ZERO.as_slice()),
            ],
            headers: vec![alloy_rlp::encode(parent).into()],
        },
        access_manifest: AccessManifest::default(),
    };

    SyntheticFixture {
        bundle,
        target,
        sender,
        recipient,
        beneficiary,
        pre_state_root,
    }
}

fn synthetic_header(
    number: u64,
    timestamp: u64,
    beneficiary: Address,
    state_root: B256,
    parent_hash: B256,
) -> Header {
    Header {
        parent_hash,
        beneficiary,
        state_root,
        number,
        gas_limit: 30_000_000,
        timestamp,
        mix_hash: B256::repeat_byte(0x77),
        base_fee_per_gas: Some(1),
        withdrawals_root: Some(EMPTY_ROOT_HASH),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::repeat_byte(0x88)),
        requests_hash: Some(B256::repeat_byte(0x99)),
        ..Default::default()
    }
}

fn synthetic_tx(sender: Address, recipient: Address) -> TxEnv {
    TxEnv {
        tx_type: 0,
        caller: sender,
        gas_limit: SUPPORTED_TX_GAS_LIMIT,
        gas_price: 2,
        kind: TxKind::Call(recipient),
        nonce: 0,
        chain_id: Some(1),
        ..Default::default()
    }
}

fn account_leaf(address: Address, account: TrieAccount) -> Vec<u8> {
    let path = Nibbles::unpack(keccak256(address));
    alloy_rlp::encode(TrieNodeV2::Leaf(LeafNode::new(
        path.slice(1..),
        alloy_rlp::encode(account),
    )))
}

fn branch_node(first: (u8, &[u8]), second: (u8, &[u8])) -> Vec<u8> {
    assert_ne!(first.0, second.0, "fixture accounts must branch at root");
    let mut children = [first, second];
    children.sort_unstable_by_key(|child| child.0);
    let stack = children
        .iter()
        .map(|(_, node)| RlpNode::from_rlp(node))
        .collect();
    let state_mask = TrieMask::new((1u16 << first.0) | (1u16 << second.0));
    alloy_rlp::encode(TrieNodeV2::Branch(BranchNodeV2::new(
        Nibbles::default(),
        stack,
        state_mask,
        None,
    )))
}

fn root_nibble(address: Address) -> u8 {
    Nibbles::unpack(keccak256(address))
        .first()
        .expect("address hash has a root nibble")
}

fn address_with_distinct_root_nibble(fill: u8, excluded: &[u8]) -> Address {
    for suffix in 0..=u8::MAX {
        let mut bytes = [fill; 20];
        bytes[19] = suffix;
        let address = Address::new(bytes);
        if !excluded.contains(&root_nibble(address)) {
            return address;
        }
    }
    panic!("failed to find deterministic address with a distinct root nibble");
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
        assert_eq!(code_bytes(actual), code_bytes(expected), "{address} code");
        assert_eq!(actual.status, expected.status, "{address} status");

        let mut actual_slots = actual.storage.keys().copied().collect::<Vec<_>>();
        let mut expected_slots = expected.storage.keys().copied().collect::<Vec<_>>();
        actual_slots.sort();
        expected_slots.sort();
        assert_eq!(actual_slots, expected_slots, "{address} storage keys");
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

fn to_revm_address(address: DtvmAddress) -> Address {
    Address::from(address.0)
}

fn to_u256(word: Word) -> U256 {
    U256::from_be_bytes(word.0)
}

fn verified_dtvm_library() -> PathBuf {
    assert_eq!(
        std::env::var("DTVM_REQUIRED").as_deref(),
        Ok("1"),
        "proof-backed differential requires DTVM_REQUIRED=1"
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
