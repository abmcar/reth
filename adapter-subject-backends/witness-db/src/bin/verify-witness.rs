use reth_dtvm_witness_db::WitnessDb;
use serde_json::json;
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    process::ExitCode,
};

fn main() -> ExitCode {
    let mut args = env::args_os();
    let program = args
        .next()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "verify-witness".to_string());
    let (path, require_target_block) = match parse_args(args) {
        Ok(args) => args,
        Err(()) => {
            eprintln!("usage: {program} [--require-target-block] BUNDLE.json");
            return ExitCode::FAILURE;
        }
    };

    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("failed to read {}: {error}", path.to_string_lossy());
            return ExitCode::FAILURE;
        }
    };
    let mut db = match WitnessDb::from_json(&bytes) {
        Ok(db) => db,
        Err(error) => {
            eprintln!("witness verification failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let raw_block_bound = db.target_block().is_some();
    if !target_block_requirement_met(require_target_block, raw_block_bound) {
        eprintln!("witness verification failed: bundle has no verified targetBlock binding");
        return ExitCode::FAILURE;
    }
    let verified_root = match db.verified_root() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("witness root verification failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let target_block_binding = if raw_block_bound {
        "rawBlock"
    } else {
        "legacyHeaderOnly"
    };

    println!(
        "{}",
        json!({
            "targetBlockNumber": db.target_header().number,
            "targetBlockHash": db.target_header().hash_slow(),
            "parentBlockNumber": db.parent_header().number,
            "parentBlockHash": db.parent_header().hash_slow(),
            "preStateRoot": verified_root,
            "accountTargets": db.access_manifest().accounts.len(),
            "storageTargets": db.access_manifest().storage.len(),
            "targetBlockBinding": target_block_binding,
            "rawBlockBound": raw_block_bound,
            "bodyCommitmentsVerified": raw_block_bound,
            "targetBlockRawBytes": db.target_block().map(|raw| raw.len()),
            "targetBlockTransactionCount": db.target_block_transaction_count(),
            "status": "verified"
        })
    );
    ExitCode::SUCCESS
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<(OsString, bool), ()> {
    let mut path = None;
    let mut require_target_block = false;
    for argument in args {
        if argument == OsStr::new("--require-target-block") {
            if require_target_block {
                return Err(());
            }
            require_target_block = true;
        } else if path.replace(argument).is_some() {
            return Err(());
        }
    }
    path.map(|path| (path, require_target_block)).ok_or(())
}

const fn target_block_requirement_met(require_target_block: bool, raw_block_bound: bool) -> bool {
    !require_target_block || raw_block_bound
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_optional_target_block_requirement() {
        assert_eq!(
            parse_args([
                OsString::from("--require-target-block"),
                OsString::from("bundle.json"),
            ]),
            Ok((OsString::from("bundle.json"), true))
        );
        assert_eq!(
            parse_args([OsString::from("bundle.json")]),
            Ok((OsString::from("bundle.json"), false))
        );
    }

    #[test]
    fn rejects_ambiguous_cli_arguments() {
        assert_eq!(parse_args([]), Err(()));
        assert_eq!(
            parse_args([OsString::from("first.json"), OsString::from("second.json"),]),
            Err(())
        );
        assert_eq!(
            parse_args([
                OsString::from("--require-target-block"),
                OsString::from("--require-target-block"),
                OsString::from("bundle.json"),
            ]),
            Err(())
        );
    }

    #[test]
    fn strict_mode_rejects_legacy_header_only_binding() {
        assert!(!target_block_requirement_met(true, false));
        assert!(target_block_requirement_met(true, true));
        assert!(target_block_requirement_met(false, false));
    }
}
