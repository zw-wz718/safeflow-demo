use safeflow_subscription_evidence::billing_adapter::normalize_bill_read;
use std::{env, fs, process::ExitCode};

fn run() -> Result<(), &'static str> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 1 {
        return Err("USAGE: normalize_billing bill-read.json");
    }
    let json = fs::read_to_string(&args[0]).map_err(|_| "INPUT_READ_FAILED")?;
    let report = normalize_bill_read(&json)?;
    let output =
        serde_json::to_string_pretty(&report).map_err(|_| "OUTPUT_SERIALIZATION_FAILED")?;
    println!("{output}");
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => {
            eprintln!("{code}");
            ExitCode::FAILURE
        }
    }
}
