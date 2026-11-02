//! Replay existing fictional evidence with separately approved local rules.
use safeflow_subscription_evidence::recognition::replay_mock;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), &'static str> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() > 1 {
        return Err("USAGE: recognize_mock [mock-fixture.json]");
    }
    let fixture = if let Some(path) = args.first() {
        std::fs::read_to_string(path).map_err(|_| "FIXTURE_READ_FAILED")?
    } else {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../fixtures/subscription/SafeFlow_SUB-01_模拟流水与预期_v0.1.json"
        ))
        .to_owned()
    };
    let result = replay_mock(&fixture)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|_| "REPORT_SERIALIZE_FAILED")?
    );
    Ok(())
}
