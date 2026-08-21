use alloy_consensus::{Header, TrieAccount};
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    eip4895::Withdrawals,
    eip7002::{WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, WITHDRAWAL_REQUEST_PREDEPLOY_CODE},
    eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
    eip7685::EMPTY_REQUESTS_HASH,
};
use alloy_evm::eth::EthEvmFactory;
use alloy_primitives::{b256, keccak256, Address, Bytes, B256, U256};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_trie::{proof::ProofRetainer, HashBuilder, Nibbles, EMPTY_ROOT_HASH};
use reth_chainspec::MAINNET;
use reth_dtvm_transaction_adapter::DbAccess;
use reth_dtvm_witness_db::{
    replay::replay_bundle, AccessManifest, WitnessBundle, WitnessDb, WitnessImportError,
};
use reth_ethereum_primitives::{Block, TransactionSigned};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{Block as _, RecoveredBlock};
use revm::database::BundleState;
use std::collections::BTreeMap;

const TARGET_NUMBER: u64 = 24_000_000;
const TARGET_TIMESTAMP: u64 = 1_800_000_000;
const HISTORY_SLOT: u64 = 369;
const BEACON_TIMESTAMP_SLOT: u64 = 3_177;
const BEACON_ROOT_SLOT: u64 = 11_368;

const CONSOLIDATION_REQUEST_CODE: &[u8; 414] = include_bytes!(
    "../../../src/execution-specs/packages/testing/src/execution_testing/forks/forks/eips/prague/contracts/consolidation_request.bin"
);

const HISTORY_CODE_HASH: B256 =
    b256!("6e49e66782037c0555897870e29fa5e552daf4719552131a0abce779daec0a5d");
const BEACON_CODE_HASH: B256 =
    b256!("f57acd40259872606d76197ef052f3d35588dadf919ee1f0e3cb9b62d3f4b02c");
const WITHDRAWAL_CODE_HASH: B256 =
    b256!("0345a365d2f4c5975b9f1599abe0a2ee76b7a3a731bc68781bd04c84e4858f50");
const CONSOLIDATION_CODE_HASH: B256 =
    b256!("78c6cb5202685228bbcbfb992b1c4e116c7ec5ef11e25b8e92716cfc628ddd60");

struct FixtureDraft {
    block: Block,
    parent: Header,
    witness: ExecutionWitness,
    pre_state_root: B256,
    parent_beacon_root: B256,
}

#[test]
fn proof_backed_empty_osaka_block_executes_canonical_system_predeploys() {
    let mut draft = fixture_draft();
    let provisional_bundle = bind_bundle(&draft.block, &draft.parent, draft.witness.clone());

    let reference_db =
        WitnessDb::from_bundle(provisional_bundle).expect("import provisional system-call witness");
    assert_eq!(
        reference_db.pre_state_root(),
        draft.pre_state_root,
        "parent header must bind the generated account trie"
    );

    let provisional_raw = alloy_rlp::encode(&draft.block);
    let recovered = recover_raw_block(&provisional_raw);
    let reference_config =
        EthEvmConfig::new_with_evm_factory(MAINNET.clone(), EthEvmFactory::default());
    let mut reference_executor = BasicBlockExecutor::new(reference_config, reference_db);
    let reference_result = reference_executor
        .execute_one(&recovered)
        .expect("stock Reth executes all four canonical system calls");
    let reference_state = reference_executor.into_state();
    let reference_accesses = reference_state.database.strict_db().accesses().to_vec();
    let reference_bundle = reference_state.bundle_state;
    let reference_db = reference_state.database;

    assert_eq!(reference_result.receipts.len(), 0);
    assert_eq!(reference_result.gas_used, 0);
    assert_eq!(reference_result.blob_gas_used, 0);
    assert!(reference_result.requests.is_empty());
    assert_eq!(
        reference_result.requests.requests_hash(),
        EMPTY_REQUESTS_HASH
    );
    assert_canonical_system_accesses(&reference_accesses);
    assert_canonical_system_state(
        &reference_bundle,
        draft.parent.hash_slow(),
        draft.parent_beacon_root,
    );

    let post_state_root = match reference_db.into_verified_post_state_root(&reference_bundle) {
        Err(WitnessImportError::PostStateRootMismatch { expected, actual }) => {
            assert_eq!(
                expected, draft.pre_state_root,
                "the provisional target deliberately carries the pre-state root"
            );
            actual
        }
        other => panic!("stock post-root binding must expose the actual root: {other:?}"),
    };
    assert_ne!(
        post_state_root, draft.pre_state_root,
        "the four system calls must change state"
    );

    draft.block.header.state_root = post_state_root;
    let final_bundle = bind_bundle(&draft.block, &draft.parent, draft.witness);
    let final_raw = final_bundle
        .target_block
        .as_ref()
        .expect("final bundle contains raw block");
    let final_recovered = recover_raw_block(final_raw);
    assert_eq!(final_recovered.header().state_root, post_state_root);
    assert_eq!(final_recovered.body().transactions.len(), 0);

    let report = replay_bundle(final_bundle).expect("strict stock/DTVM system-call replay");
    assert_eq!(report.differential_match, Some(true));
    assert!(report.raw_bound);
    assert!(report.pre_execution_commitments);
    assert!(report.pre_state_root_verified);
    assert!(report.post_state_root_verified);
    assert_eq!(report.pre_state_root, draft.pre_state_root);
    assert_eq!(report.post_state_root, post_state_root);
    assert_eq!(report.block_number, TARGET_NUMBER);
    assert_eq!(report.transaction_count, 0);
    assert_eq!(report.receipt_count, 0);
    assert_eq!(report.gas_used, 0);
    assert_eq!(report.blob_gas_used, 0);
    assert!(report.post_execution_commitments.gas_used);
    assert!(report.post_execution_commitments.receipts_root);
    assert!(report.post_execution_commitments.logs_bloom);
    assert!(report.post_execution_commitments.requests_hash);
    assert!(report.post_execution_commitments.blob_gas_used);
}

fn fixture_draft() -> FixtureDraft {
    let codes = canonical_system_codes();
    let (withdrawal_storage_root, withdrawal_storage_nodes) = inhibitor_storage_proof();
    let (consolidation_storage_root, consolidation_storage_nodes) = inhibitor_storage_proof();
    assert_eq!(withdrawal_storage_root, consolidation_storage_root);

    let accounts = [
        (
            HISTORY_STORAGE_ADDRESS,
            TrieAccount {
                nonce: 1,
                balance: U256::ZERO,
                storage_root: EMPTY_ROOT_HASH,
                code_hash: HISTORY_CODE_HASH,
            },
        ),
        (
            BEACON_ROOTS_ADDRESS,
            TrieAccount {
                nonce: 1,
                balance: U256::ZERO,
                storage_root: EMPTY_ROOT_HASH,
                code_hash: BEACON_CODE_HASH,
            },
        ),
        (
            WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
            TrieAccount {
                nonce: 1,
                balance: U256::ZERO,
                storage_root: withdrawal_storage_root,
                code_hash: WITHDRAWAL_CODE_HASH,
            },
        ),
        (
            CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
            TrieAccount {
                nonce: 1,
                balance: U256::ZERO,
                storage_root: consolidation_storage_root,
                code_hash: CONSOLIDATION_CODE_HASH,
            },
        ),
    ];
    let (pre_state_root, account_nodes) = account_proof(accounts);

    let mut unique_nodes = BTreeMap::new();
    for node in account_nodes
        .into_iter()
        .chain(withdrawal_storage_nodes)
        .chain(consolidation_storage_nodes)
    {
        unique_nodes.entry(keccak256(&node)).or_insert(node);
    }

    let parent_beacon_root = B256::repeat_byte(0x88);
    let parent = osaka_header(
        TARGET_NUMBER - 1,
        TARGET_TIMESTAMP - 12,
        pre_state_root,
        B256::repeat_byte(0x44),
        B256::repeat_byte(0x77),
    );
    let target = osaka_header(
        TARGET_NUMBER,
        TARGET_TIMESTAMP,
        pre_state_root,
        parent.hash_slow(),
        parent_beacon_root,
    );
    let mut block = Block::from_transactions(target, Vec::<TransactionSigned>::new());
    block.body.withdrawals = Some(Withdrawals::default());
    block.header.withdrawals_root = block.body.calculate_withdrawals_root();

    let mut keys = [
        HISTORY_STORAGE_ADDRESS,
        BEACON_ROOTS_ADDRESS,
        WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
        CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
    ]
    .into_iter()
    .map(|address| Bytes::copy_from_slice(address.as_slice()))
    .collect::<Vec<_>>();
    keys.extend((0..=3).map(|slot| Bytes::copy_from_slice(slot_key(slot).as_slice())));

    FixtureDraft {
        block,
        parent,
        witness: ExecutionWitness {
            state: unique_nodes.into_values().collect(),
            codes: codes.into_iter().map(|(_, code)| code).collect(),
            keys,
            headers: Vec::new(),
        },
        pre_state_root,
        parent_beacon_root,
    }
}

fn canonical_system_codes() -> [(Address, Bytes); 4] {
    let consolidation = Bytes::from_static(CONSOLIDATION_REQUEST_CODE);
    let codes = [
        (HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone()),
        (BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone()),
        (
            WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
            WITHDRAWAL_REQUEST_PREDEPLOY_CODE.clone(),
        ),
        (CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS, consolidation),
    ];
    let expected_hashes = [
        HISTORY_CODE_HASH,
        BEACON_CODE_HASH,
        WITHDRAWAL_CODE_HASH,
        CONSOLIDATION_CODE_HASH,
    ];
    for ((_, code), expected_hash) in codes.iter().zip(expected_hashes) {
        assert_eq!(keccak256(code), expected_hash);
    }
    assert_eq!(codes[3].1.len(), 414);
    assert_eq!(
        &codes[3].1[codes[3].1.len() - 4..],
        &[0x5b, 0x5f, 0x5f, 0xfd]
    );
    codes
}

fn inhibitor_storage_proof() -> (B256, Vec<Bytes>) {
    let targets = (0..=3)
        .map(|slot| Nibbles::unpack(keccak256(slot_key(slot))))
        .collect::<Vec<_>>();
    let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::from_iter(targets));
    let value = alloy_rlp::encode_fixed_size(&U256::MAX);
    builder.add_leaf(Nibbles::unpack(keccak256(slot_key(0))), &value);
    let root = builder.root();
    let nodes = builder
        .take_proof_nodes()
        .into_nodes_sorted()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    (root, nodes)
}

fn account_proof(accounts: [(Address, TrieAccount); 4]) -> (B256, Vec<Bytes>) {
    let mut leaves = accounts
        .into_iter()
        .map(|(address, account)| {
            (
                Nibbles::unpack(keccak256(address)),
                alloy_rlp::encode(account),
            )
        })
        .collect::<Vec<_>>();
    leaves.sort_unstable_by_key(|(path, _)| *path);
    let targets = leaves.iter().map(|(path, _)| *path).collect::<Vec<_>>();
    let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::from_iter(targets));
    for (path, account) in leaves {
        builder.add_leaf(path, &account);
    }
    let root = builder.root();
    let nodes = builder
        .take_proof_nodes()
        .into_nodes_sorted()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    (root, nodes)
}

fn osaka_header(
    number: u64,
    timestamp: u64,
    state_root: B256,
    parent_hash: B256,
    parent_beacon_root: B256,
) -> Header {
    Header {
        parent_hash,
        beneficiary: Address::ZERO,
        state_root,
        number,
        gas_limit: 30_000_000,
        timestamp,
        mix_hash: B256::repeat_byte(0x66),
        base_fee_per_gas: Some(1),
        withdrawals_root: Some(EMPTY_ROOT_HASH),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(parent_beacon_root),
        requests_hash: Some(EMPTY_REQUESTS_HASH),
        block_access_list_hash: None,
        slot_number: None,
        ..Default::default()
    }
}

fn bind_bundle(block: &Block, parent: &Header, mut witness: ExecutionWitness) -> WitnessBundle {
    let encoded_block = alloy_rlp::encode(block);
    let mut raw = encoded_block.as_slice();
    let sealed = Block::decode_sealed(&mut raw).expect("decode generated raw block");
    assert!(raw.is_empty(), "generated raw block has no trailing bytes");
    assert_eq!(sealed.header(), &block.header);

    witness.headers = vec![alloy_rlp::encode(parent).into()];
    WitnessBundle {
        target_header: alloy_rlp::encode(&block.header).into(),
        target_block_hash: sealed.hash(),
        target_block: Some(encoded_block.into()),
        witness,
        access_manifest: AccessManifest::default(),
    }
}

fn recover_raw_block(raw: &[u8]) -> RecoveredBlock<Block> {
    let mut input = raw;
    let sealed = Block::decode_sealed(&mut input).expect("decode bound raw block");
    assert!(input.is_empty(), "bound raw block has no trailing bytes");
    RecoveredBlock::try_recover_sealed(sealed.into()).expect("recover empty raw block")
}

fn assert_canonical_system_accesses(accesses: &[DbAccess]) {
    let code_hashes = accesses
        .iter()
        .filter_map(|access| match access {
            DbAccess::Code(code_hash) => Some(*code_hash),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        code_hashes,
        [
            HISTORY_CODE_HASH,
            BEACON_CODE_HASH,
            WITHDRAWAL_CODE_HASH,
            CONSOLIDATION_CODE_HASH,
        ],
        "each production predeploy must execute in canonical block order"
    );

    assert_eq!(
        storage_slots(accesses, HISTORY_STORAGE_ADDRESS),
        [U256::from(HISTORY_SLOT)]
    );
    assert_eq!(
        storage_slots(accesses, BEACON_ROOTS_ADDRESS),
        [
            U256::from(BEACON_TIMESTAMP_SLOT),
            U256::from(BEACON_ROOT_SLOT),
        ]
    );
    let request_slots = [U256::from(3), U256::from(2), U256::ZERO, U256::from(1)];
    assert_eq!(
        storage_slots(accesses, WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS),
        request_slots
    );
    assert_eq!(
        storage_slots(accesses, CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS),
        request_slots
    );
}

fn storage_slots(accesses: &[DbAccess], target: Address) -> Vec<U256> {
    accesses
        .iter()
        .filter_map(|access| match access {
            DbAccess::Storage(address, slot) | DbAccess::StorageByAccountId(address, _, slot)
                if *address == target =>
            {
                Some(*slot)
            }
            _ => None,
        })
        .collect()
}

fn assert_canonical_system_state(state: &BundleState, parent_hash: B256, parent_beacon_root: B256) {
    assert_eq!(
        bundle_storage(state, HISTORY_STORAGE_ADDRESS, HISTORY_SLOT),
        U256::from_be_bytes(parent_hash.0)
    );
    assert_eq!(
        bundle_storage(state, BEACON_ROOTS_ADDRESS, BEACON_TIMESTAMP_SLOT),
        U256::from(TARGET_TIMESTAMP)
    );
    assert_eq!(
        bundle_storage(state, BEACON_ROOTS_ADDRESS, BEACON_ROOT_SLOT),
        U256::from_be_bytes(parent_beacon_root.0)
    );
    for address in [
        WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
        CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
    ] {
        let account = state
            .state
            .get(&address)
            .unwrap_or_else(|| panic!("missing changed request account {address}"));
        assert_eq!(
            account.storage.len(),
            1,
            "only the nonzero inhibitor leaf may change for {address}"
        );
        assert_eq!(
            account.storage_slot(U256::ZERO),
            Some(U256::ZERO),
            "{address} must clear its sole inhibitor leaf, producing an empty storage trie"
        );
    }
}

fn bundle_storage(state: &BundleState, address: Address, slot: u64) -> U256 {
    state
        .state
        .get(&address)
        .unwrap_or_else(|| panic!("missing changed account {address}"))
        .storage_slot(U256::from(slot))
        .unwrap_or_else(|| panic!("missing touched slot {address}[{slot}]"))
}

fn slot_key(slot: u64) -> B256 {
    B256::from(U256::from(slot).to_be_bytes::<32>())
}
