use reth_dtvm_witness_db::replay::{replay_bundle_json_with_mode, ReplayMode};
use std::{env, fs, process::ExitCode};

fn main() -> ExitCode {
    let mut args = env::args_os();
    let program = args
        .next()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "replay-block".to_string());
    let Some(first) = args.next() else {
        print_usage(&program);
        return ExitCode::FAILURE;
    };
    let (mode, path) = if first == "--mode" {
        let Some(value) = args.next().and_then(|value| value.into_string().ok()) else {
            print_usage(&program);
            return ExitCode::FAILURE;
        };
        let Some(mode) = ReplayMode::parse(&value) else {
            eprintln!("invalid replay mode {value:?}");
            print_usage(&program);
            return ExitCode::FAILURE;
        };
        let Some(path) = args.next() else {
            print_usage(&program);
            return ExitCode::FAILURE;
        };
        (mode, path)
    } else {
        (ReplayMode::Differential, first)
    };
    if args.next().is_some() {
        print_usage(&program);
        return ExitCode::FAILURE;
    }

    let json = match fs::read(&path) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("failed to read {}: {error}", path.to_string_lossy());
            return ExitCode::FAILURE;
        }
    };
    let report = match replay_bundle_json_with_mode(&json, mode) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("strict {} replay failed: {error}", mode.as_str());
            return ExitCode::FAILURE;
        }
    };
    match serde_json::to_string(&report) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("failed to serialize replay report: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage(program: &str) {
    eprintln!("usage: {program} [--mode differential|reference-only|subject-only] BUNDLE.json");
    eprintln!(
        "subject modes require: RETH_SUBJECT_BACKEND=dtvm-eager|dtvm-profile-guided|\
         evmone-advanced, RETH_SUBJECT_LIBRARY, RETH_SUBJECT_LIBRARY_SHA256"
    );
    eprintln!(
        "DTVM subjects additionally require \
         DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true"
    );
}
