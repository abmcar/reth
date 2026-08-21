use reth_dtvm_witness_db::{
    batch::BatchInput,
    evmone_batch::{run_evmone_smoke, run_fixed_evmone_batch, run_fixed_evmone_resource_batch},
};
use std::{env, fs, io, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let mut args = env::args_os();
    let program = args
        .next()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "replay-evmone-batch".to_string());
    let mut smoke = false;
    let mut resource_only = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg == "--smoke-one" {
            smoke = true;
        } else if arg == "--resource-only" {
            resource_only = true;
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    if paths.is_empty() {
        print_usage(&program);
        return ExitCode::FAILURE;
    }
    let mut inputs = Vec::with_capacity(paths.len());
    for path in paths {
        let json = match fs::read(&path) {
            Ok(json) => json,
            Err(error) => {
                eprintln!("failed to read {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        };
        inputs.push(BatchInput { path, json });
    }
    let stdout = io::stdout();
    let result = if smoke && resource_only {
        Err(reth_dtvm_witness_db::evmone_batch::EvmoneBatchError::ConflictingModes)
    } else if smoke {
        run_evmone_smoke(&inputs, stdout.lock())
    } else if resource_only {
        run_fixed_evmone_resource_batch(&inputs, stdout.lock())
    } else {
        run_fixed_evmone_batch(&inputs, stdout.lock())
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fixed evmone diagnostic replay batch failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage(program: &str) {
    eprintln!("usage: {program} [--smoke-one|--resource-only] BUNDLE.json [BUNDLE.json ...]");
    eprintln!(
        "requires RETH_SUBJECT_BACKEND=evmone-advanced, the diagnostic libevmone, \
         RETH_SUBJECT_LIBRARY, and RETH_SUBJECT_LIBRARY_SHA256"
    );
}
