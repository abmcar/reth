use reth_dtvm_witness_db::batch::{
    run_cold_hot_gate_pilot, run_fixed_hot_batch, run_fixed_production_batch,
    run_fixed_production_resource_batch, run_fixed_resource_batch, BatchInput, ResourceEngine,
};
use std::{env, fs, io, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let mut args = env::args_os();
    let program = args
        .next()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "replay-batch".to_string());
    let mut resource_engine = None;
    let mut pilot_cold_hot_gate = false;
    let mut production_timing = false;
    let mut production_resource = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg == "--resource-engine=dtvm" {
            resource_engine = Some(ResourceEngine::Dtvm);
        } else if arg == "--resource-engine=revm" {
            resource_engine = Some(ResourceEngine::Revm);
        } else if arg == "--pilot-cold-hot-gate" {
            pilot_cold_hot_gate = true;
        } else if arg == "--production-timing" {
            production_timing = true;
        } else if arg == "--resource-engine=dtvm-production" {
            production_resource = true;
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    if usize::from(pilot_cold_hot_gate)
        + usize::from(production_timing)
        + usize::from(production_resource)
        + usize::from(resource_engine.is_some())
        > 1
    {
        eprintln!(
            "pilot, production timing/resource, and diagnostic resource modes are mutually exclusive"
        );
        print_usage(&program);
        return ExitCode::FAILURE;
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
    let result = match resource_engine {
        Some(engine) => run_fixed_resource_batch(&inputs, engine, stdout.lock()),
        None if pilot_cold_hot_gate => run_cold_hot_gate_pilot(&inputs, stdout.lock()),
        None if production_timing => run_fixed_production_batch(&inputs, stdout.lock()),
        None if production_resource => run_fixed_production_resource_batch(&inputs, stdout.lock()),
        None => run_fixed_hot_batch(&inputs, stdout.lock()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fixed hot-cache replay batch failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage(program: &str) {
    eprintln!(
        "usage: {program} [--pilot-cold-hot-gate | --production-timing | --resource-engine=dtvm-production|dtvm|revm] \
         BUNDLE.json [BUNDLE.json ...]"
    );
    eprintln!(
        "requires RETH_SUBJECT_BACKEND=dtvm-eager, RETH_SUBJECT_LIBRARY, \
         RETH_SUBJECT_LIBRARY_SHA256, DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true, \
         plus DTVM metrics ABI v2 except that --production-timing requires a metrics-OFF library"
    );
}
