use alloy_consensus::{Header, SignableTransaction, TrieAccount, TxLegacy};
use alloy_eips::{
    eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS, eip4895::Withdrawals,
    eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
    eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS, eip7685::EMPTY_REQUESTS_HASH,
};
use alloy_evm::eth::EthEvmFactory;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_trie::{TrieMask, EMPTY_ROOT_HASH, KECCAK_EMPTY};
use reth_chainspec::MAINNET;
use reth_dtvm_transaction_adapter::{
    DbAccess, DtvmEvmFactory, STORAGE_LOG_RETURN, SUPPORTED_TX_GAS_LIMIT,
};
use reth_dtvm_witness_db::{AccessManifest, WitnessBundle, WitnessDb};
use reth_ethereum_primitives::{Block, Transaction, TransactionSigned};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{
    crypto::secp256k1::sign_message, Block as _, RecoveredBlock, SignedTransaction,
};
use reth_trie_common::{BranchNodeV2, LeafNode, Nibbles, RlpNode, TrieNodeV2};
use revm::{database::BundleState, Database};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::PathBuf};

const TARGET_NUMBER: u64 = 24_000_000;
const TARGET_TIMESTAMP: u64 = 1_800_000_000;

struct SyntheticBlockFixture {
    bundle: WitnessBundle,
    encoded_block: Vec<u8>,
    expected_header: Header,
    expected_hash: B256,
    sender: Address,
    recipient: Address,
    beneficiary: Address,
    pre_state_root: B256,
}

/// This is a synthetic signed raw one-block differential fixture.
///
/// It covers canonical RLP decoding, sender recovery, block-executor
/// orchestration, and proven-absent system-predeploy empty-code paths. It does
/// not claim canonical predeploy bytecode, consensus header commitments, or a
/// post-state root. In particular, its placeholder gas-used, receipts-root,
/// and state-root fields are not claimed to be valid.
#[test]
fn proof_backed_signed_raw_osaka_block_matches_stock_reth_and_real_dtvm() {
    let fixture = synthetic_block_fixture();
    assert_eq!(fixture.bundle.access_manifest, AccessManifest::default());
    let mut encoded = fixture.encoded_block.as_slice();
    let sealed = Block::decode_sealed(&mut encoded).expect("decode synthetic raw block RLP");
    assert!(
        encoded.is_empty(),
        "raw block RLP must have no trailing bytes"
    );
    assert_eq!(sealed.header(), &fixture.expected_header);
    assert_eq!(sealed.hash(), fixture.expected_hash);

    let recovered =
        RecoveredBlock::try_recover_sealed(sealed.into()).expect("recover synthetic block sender");
    assert_eq!(recovered.senders(), &[fixture.sender]);

    assert_complete_trie_accounts(&fixture);

    let mut reference_db =
        WitnessDb::from_bundle(fixture.bundle.clone()).expect("stock proof-backed block database");
    let mut dtvm_db =
        WitnessDb::from_bundle(fixture.bundle.clone()).expect("DTVM proof-backed block database");
    assert_eq!(
        reference_db.target_block().map(Bytes::as_ref),
        Some(fixture.encoded_block.as_slice())
    );
    assert_eq!(
        dtvm_db.target_block().map(Bytes::as_ref),
        Some(fixture.encoded_block.as_slice())
    );
    assert_eq!(reference_db.target_block_transaction_count(), Some(1));
    assert_eq!(dtvm_db.target_block_transaction_count(), Some(1));
    assert_eq!(
        reference_db.verified_root().unwrap(),
        fixture.pre_state_root
    );
    assert_eq!(dtvm_db.verified_root().unwrap(), fixture.pre_state_root);

    let reference_config =
        EthEvmConfig::new_with_evm_factory(MAINNET.clone(), EthEvmFactory::default());
    let mut reference_executor = BasicBlockExecutor::new(reference_config, reference_db);
    let reference_result = reference_executor
        .execute_one(&recovered)
        .expect("stock reth executes proof-backed signed block");
    let reference_state = reference_executor.into_state();
    let reference_accesses = reference_state.database.strict_db().accesses().to_vec();
    let reference_bundle = reference_state.bundle_state;

    let dtvm_config = EthEvmConfig::new_with_evm_factory(
        MAINNET.clone(),
        DtvmEvmFactory::new(verified_dtvm_library()),
    );
    let mut dtvm_executor = BasicBlockExecutor::new(dtvm_config, dtvm_db);
    let dtvm_result = dtvm_executor
        .execute_one(&recovered)
        .expect("real DTVM executes proof-backed signed block");
    let dtvm_state = dtvm_executor.into_state();
    let dtvm_accesses = dtvm_state.database.strict_db().accesses().to_vec();
    let dtvm_bundle = dtvm_state.bundle_state;

    assert_eq!(
        dtvm_result, reference_result,
        "complete BlockExecutionResult must match"
    );
    assert_bundle_semantics_eq(&dtvm_bundle, &reference_bundle);
    assert_eq!(dtvm_accesses, reference_accesses);
    assert_eq!(reference_result.receipts.len(), 1);
    assert!(reference_result.receipts[0].success);
    assert_eq!(reference_accesses, expected_accesses(&fixture));
}

fn synthetic_block_fixture() -> SyntheticBlockFixture {
    let (signed, sender, recipient) = signed_call();
    let sender_nibble = root_nibble(sender);
    let recipient_nibble = root_nibble(recipient);
    assert_ne!(sender_nibble, recipient_nibble);
    let beneficiary = Address::new([0x33; 20]);

    let sender_node = account_leaf(
        sender,
        TrieAccount {
            nonce: 0,
            balance: U256::from(10_000_000u64),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: KECCAK_EMPTY,
        },
    );
    let recipient_node = account_leaf(
        recipient,
        TrieAccount {
            nonce: 1,
            balance: U256::ZERO,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: keccak256(STORAGE_LOG_RETURN),
        },
    );
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
    assert_eq!(parent.block_access_list_hash, None);
    assert_eq!(parent.slot_number, None);
    let mut block = Block::from_transactions(
        synthetic_header(
            TARGET_NUMBER,
            TARGET_TIMESTAMP,
            beneficiary,
            EMPTY_ROOT_HASH,
            parent.hash_slow(),
        ),
        [signed],
    );
    block.body.withdrawals = Some(Withdrawals::default());
    block.header.withdrawals_root = block.body.calculate_withdrawals_root();
    assert_eq!(block.header.block_access_list_hash, None);
    assert_eq!(block.header.slot_number, None);
    let constructed_header = block.header.clone();
    let constructed_hash = constructed_header.hash_slow();
    let encoded_block = alloy_rlp::encode(&block);
    let mut raw = encoded_block.as_slice();
    let decoded = Block::decode_sealed(&mut raw).expect("decode fixture block before witness bind");
    assert!(raw.is_empty(), "fixture block must be exact canonical RLP");
    assert_eq!(decoded.header(), &constructed_header);
    assert_eq!(decoded.hash(), constructed_hash);
    let expected_header = decoded.header().clone();
    let expected_hash = decoded.hash();

    let bundle = WitnessBundle {
        target_header: alloy_rlp::encode(&expected_header).into(),
        target_block_hash: expected_hash,
        target_block: Some(encoded_block.clone().into()),
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

    SyntheticBlockFixture {
        bundle,
        encoded_block,
        expected_header,
        expected_hash,
        sender,
        recipient,
        beneficiary,
        pre_state_root,
    }
}

fn signed_call() -> (TransactionSigned, Address, Address) {
    let secret = B256::repeat_byte(0x46);
    for suffix in 0..=u8::MAX {
        let mut recipient_bytes = [0x22; 20];
        recipient_bytes[19] = suffix;
        let recipient = Address::new(recipient_bytes);
        let tx = Transaction::Legacy(TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 2,
            gas_limit: SUPPORTED_TX_GAS_LIMIT,
            to: TxKind::Call(recipient),
            value: U256::ZERO,
            input: Bytes::new(),
        });
        let signature =
            sign_message(secret, tx.signature_hash()).expect("sign deterministic transaction");
        let signed: TransactionSigned = tx.into_signed(signature).into();
        let sender = signed
            .try_recover()
            .expect("recover deterministic transaction signer");
        if root_nibble(sender) != root_nibble(recipient) {
            return (signed, sender, recipient);
        }
    }
    panic!("failed to find recipient with a root nibble distinct from sender");
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
        requests_hash: Some(EMPTY_REQUESTS_HASH),
        block_access_list_hash: None,
        slot_number: None,
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

fn expected_accesses(fixture: &SyntheticBlockFixture) -> Vec<DbAccess> {
    vec![
        DbAccess::Basic(HISTORY_STORAGE_ADDRESS),
        DbAccess::Basic(BEACON_ROOTS_ADDRESS),
        DbAccess::Basic(fixture.sender),
        DbAccess::Basic(fixture.recipient),
        DbAccess::Code(keccak256(STORAGE_LOG_RETURN)),
        DbAccess::Storage(fixture.recipient, U256::ZERO),
        DbAccess::Basic(fixture.beneficiary),
        DbAccess::Basic(WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS),
        DbAccess::Basic(CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS),
    ]
}

fn assert_complete_trie_accounts(fixture: &SyntheticBlockFixture) {
    let mut proof = WitnessDb::from_bundle(fixture.bundle.clone())
        .expect("proof database for fixture-account assertions");
    assert!(proof
        .basic(fixture.sender)
        .expect("prove sender account")
        .is_some());
    assert!(proof
        .basic(fixture.recipient)
        .expect("prove recipient account")
        .is_some());

    for address in [
        HISTORY_STORAGE_ADDRESS,
        BEACON_ROOTS_ADDRESS,
        WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
        CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
        fixture.beneficiary,
    ] {
        assert_eq!(
            proof
                .basic(address)
                .expect("prove synthetic account absence"),
            None,
            "{address} must be proven absent by the complete two-leaf trie"
        );
    }
}

fn assert_bundle_semantics_eq(actual: &BundleState, expected: &BundleState) {
    assert_eq!(actual.state, expected.state, "bundle account state");
    assert_eq!(actual.contracts, expected.contracts, "bundle contracts");
    assert!(
        actual.reverts.content_eq(&expected.reverts),
        "bundle revert semantics differ: actual={actual:#?}, expected={expected:#?}"
    );
}

fn verified_dtvm_library() -> PathBuf {
    assert_eq!(
        std::env::var("DTVM_REQUIRED").as_deref(),
        Ok("1"),
        "proof-backed block differential requires DTVM_REQUIRED=1"
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
