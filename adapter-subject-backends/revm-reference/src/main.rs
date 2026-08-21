fn main() {
    match revm_osaka_reference::run_reference() {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .expect("serializing the fixed REVM report cannot fail")
            );
        }
        Err(error) => {
            eprintln!("independent REVM reference failed: {error}");
            std::process::exit(1);
        }
    }
}

