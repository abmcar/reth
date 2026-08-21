//! Independent REVM reference for one Osaka VM slice.
//!
//! This crate does not load DTVM and does not compare against DTVM output.

use alloy_evm::{Evm, EvmEnv, EvmFactory, eth::EthEvmFactory};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use revm::{
    bytecode::Bytecode,
    context::TxEnv,
    context_interface::result::ExecutionResult,
    database::{CacheDB, EmptyDB},
    primitives::hardfork::SpecId,
    state::AccountInfo,
};
use revm_inspectors::access_list::AccessListInspector;
use serde::Serialize;
use std::fmt::Write;

pub const TX_GAS_LIMIT: u64 = 1_021_000;
pub const INTRINSIC_GAS: u64 = 21_000;
pub const INITIAL_FRAME_GAS: u64 = 1_000_000;

pub const RECIPIENT: Address = Address::new([0x11; 20]);
pub const SENDER: Address = Address::new([0x22; 20]);

/// PUSH1 0x2a PUSH0 SSTORE
/// PUSH1 0x2a PUSH0 MSTORE
/// PUSH1 0x01 PUSH1 0x20 PUSH0 LOG1
/// PUSH0 SLOAD PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
pub const STORAGE_LOG_RETURN: &[u8] = &[
    0x60, 0x2a, 0x5f, 0x55, 0x60, 0x2a, 0x5f, 0x52, 0x60, 0x01, 0x60, 0x20, 0x5f, 0xa1, 0x5f,
    0x54, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
];

#[derive(Debug, Serialize)]
pub struct ReferenceReport {
    pub schema_version: u32,
    pub claim_scope: String,
    pub explicit_non_claims: Vec<String>,
    pub source: SourceReport,
    pub fixture: FixtureReport,
    pub execution: ExecutionReport,
    pub state: StateReport,
    pub logs: Vec<LogReport>,
    pub access_list_inspector: AccessListReport,
}

#[derive(Debug, Serialize)]
pub struct SourceReport {
    pub alloy_evm: String,
    pub revm: String,
    pub revm_inspectors: String,
    pub spec_id: String,
}

#[derive(Debug, Serialize)]
pub struct FixtureReport {
    pub sender: String,
    pub recipient: String,
    pub bytecode: String,
    pub calldata: String,
    pub value: String,
    pub initial_slot0: String,
    pub tx_gas_limit: u64,
    pub intrinsic_gas: u64,
    pub initial_frame_gas: u64,
}

#[derive(Debug, Serialize)]
pub struct ExecutionReport {
    pub status: String,
    pub success_reason: String,
    pub output: String,
    pub total_gas_used: u64,
    pub tx_gas_used: u64,
    pub intrinsic_gas: u64,
    pub frame_gas_used: u64,
    pub gas_refund: u64,
}

#[derive(Debug, Serialize)]
pub struct StateReport {
    pub slot0: String,
}

#[derive(Debug, Serialize)]
pub struct LogReport {
    pub address: String,
    pub data: String,
    pub topics: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AccessListReport {
    pub status: String,
    pub semantics: String,
    pub full_access_audit_status: String,
    pub full_access_audit_blocker: String,
    pub items: Vec<AccessListItemReport>,
}

#[derive(Debug, Serialize)]
pub struct AccessListItemReport {
    pub address: String,
    pub storage_keys: Vec<String>,
}

pub fn run_reference() -> Result<ReferenceReport, String> {
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(
        SENDER,
        AccountInfo {
            balance: U256::from(10_000_000u64),
            ..Default::default()
        },
    );
    db.insert_account_info(
        RECIPIENT,
        AccountInfo::default().with_code(Bytecode::new_raw(Bytes::copy_from_slice(
            STORAGE_LOG_RETURN,
        ))),
    );
    db.insert_account_storage(RECIPIENT, U256::ZERO, U256::ZERO)
        .map_err(|error| format!("failed to initialize slot0: {error:?}"))?;

    let mut env = EvmEnv::default();
    env.cfg_env.spec = SpecId::OSAKA;
    env.cfg_env.chain_id = 1;

    let inspector = AccessListInspector::new(Default::default());
    let mut evm =
        EthEvmFactory::default().create_evm_with_inspector(db, env, inspector);
    let outcome = evm
        .transact_raw(TxEnv {
            caller: SENDER,
            gas_limit: TX_GAS_LIMIT,
            kind: TxKind::Call(RECIPIENT),
            chain_id: Some(1),
            ..Default::default()
        })
        .map_err(|error| format!("REVM transaction error: {error:?}"))?;

    let access_list = evm.inspector().access_list();
    let mut access_items = access_list
        .0
        .into_iter()
        .map(|item| {
            let mut storage_keys = item
                .storage_keys
                .iter()
                .map(|key| encode_hex(key.as_slice()))
                .collect::<Vec<_>>();
            storage_keys.sort();
            AccessListItemReport {
                address: encode_hex(item.address.as_slice()),
                storage_keys,
            }
        })
        .collect::<Vec<_>>();
    access_items.sort_by(|left, right| left.address.cmp(&right.address));

    let slot0 = outcome
        .state
        .get(&RECIPIENT)
        .and_then(|account| account.storage.get(&U256::ZERO))
        .ok_or_else(|| "REVM output state did not contain recipient slot0".to_string())?
        .present_value();

    let ExecutionResult::Success {
        reason,
        gas,
        logs,
        output,
    } = outcome.result
    else {
        return Err(format!("REVM execution did not succeed: {:?}", outcome.result));
    };

    let total_gas_used = gas.total_gas_spent();
    let frame_gas_used = total_gas_used
        .checked_sub(INTRINSIC_GAS)
        .ok_or_else(|| "total gas used was below intrinsic gas".to_string())?;

    let logs = logs
        .into_iter()
        .map(|log| LogReport {
            address: encode_hex(log.address.as_slice()),
            data: encode_hex(log.data.data.as_ref()),
            topics: log
                .data
                .topics()
                .iter()
                .map(|topic| encode_hex(topic.as_slice()))
                .collect(),
        })
        .collect::<Vec<_>>();

    let report = ReferenceReport {
        schema_version: 1,
        claim_scope: "independent REVM Osaka baseline only".to_string(),
        explicit_non_claims: vec![
            "DTVM was not loaded or executed".to_string(),
            "No DTVM result was read".to_string(),
            "No differential PASS is claimed".to_string(),
            "No block correctness is claimed".to_string(),
        ],
        source: SourceReport {
            alloy_evm: "0.37.1".to_string(),
            revm: "41.0.0".to_string(),
            revm_inspectors: "0.41.2".to_string(),
            spec_id: "OSAKA".to_string(),
        },
        fixture: FixtureReport {
            sender: encode_hex(SENDER.as_slice()),
            recipient: encode_hex(RECIPIENT.as_slice()),
            bytecode: encode_hex(STORAGE_LOG_RETURN),
            calldata: "0x".to_string(),
            value: word_hex(U256::ZERO),
            initial_slot0: word_hex(U256::ZERO),
            tx_gas_limit: TX_GAS_LIMIT,
            intrinsic_gas: INTRINSIC_GAS,
            initial_frame_gas: INITIAL_FRAME_GAS,
        },
        execution: ExecutionReport {
            status: "success".to_string(),
            success_reason: format!("{reason:?}"),
            output: encode_hex(output.data().as_ref()),
            total_gas_used,
            tx_gas_used: gas.tx_gas_used(),
            intrinsic_gas: INTRINSIC_GAS,
            frame_gas_used,
            gas_refund: gas.inner_refunded(),
        },
        state: StateReport {
            slot0: word_hex(slot0),
        },
        logs,
        access_list_inspector: AccessListReport {
            status: "collected".to_string(),
            semantics: "EIP-2930 access-list candidates from revm-inspectors AccessListInspector; sender, top-level recipient, precompiles, and EIP-7702 authorities are excluded as addresses by inspector design, while touched storage slots remain represented".to_string(),
            full_access_audit_status: "blocked".to_string(),
            full_access_audit_blocker: "AccessListInspector does not expose a complete ordered read/write audit or multiplicity, so its output must not be treated as the full witness access trace".to_string(),
            items: access_items,
        },
    };

    validate_report(&report)?;
    Ok(report)
}

fn validate_report(report: &ReferenceReport) -> Result<(), String> {
    let expected_word =
        "0x000000000000000000000000000000000000000000000000000000000000002a";
    let expected_topic =
        "0x0000000000000000000000000000000000000000000000000000000000000001";
    let recipient = encode_hex(RECIPIENT.as_slice());
    let slot0 =
        "0x0000000000000000000000000000000000000000000000000000000000000000";

    if report.execution.output != expected_word {
        return Err(format!("unexpected output: {}", report.execution.output));
    }
    if report.state.slot0 != expected_word {
        return Err(format!("unexpected slot0: {}", report.state.slot0));
    }
    if report.fixture.tx_gas_limit - report.fixture.intrinsic_gas != INITIAL_FRAME_GAS {
        return Err("fixture does not provide exactly 1,000,000 initial frame gas".to_string());
    }
    if report.logs.len() != 1 {
        return Err(format!("expected one log, got {}", report.logs.len()));
    }
    if report.logs[0].address != recipient
        || report.logs[0].data != expected_word
        || report.logs[0].topics != [expected_topic]
    {
        return Err("unexpected log address, data, or topics".to_string());
    }
    if report.access_list_inspector.items.len() != 1
        || report.access_list_inspector.items[0].address != recipient
        || report.access_list_inspector.items[0].storage_keys != [slot0]
    {
        return Err("unexpected AccessListInspector output".to_string());
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(2 + bytes.len() * 2);
    output.push_str("0x");
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn word_hex(value: U256) -> String {
    encode_hex(&value.to_be_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_log_return_osaka_reference_is_self_consistent() {
        let report = run_reference().expect("independent REVM reference must execute");

        assert_eq!(report.source.spec_id, "OSAKA");
        assert_eq!(report.execution.status, "success");
        assert_eq!(
            report.fixture.tx_gas_limit - report.fixture.intrinsic_gas,
            INITIAL_FRAME_GAS
        );
        assert_eq!(report.logs.len(), 1);
        assert_eq!(report.access_list_inspector.status, "collected");
        assert_eq!(
            report.access_list_inspector.full_access_audit_status,
            "blocked"
        );
    }
}
