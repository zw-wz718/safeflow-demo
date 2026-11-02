use safeflow_subscription_evidence::peer_review::inspect_transfer_fixture;

fn run() -> Result<(), &'static str> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 1 {
        return Err("USAGE: inspect_transfer_fixture <peer-mock.json>");
    }
    let input = std::fs::read_to_string(&args[0]).map_err(|_| "PEER_FIXTURE_READ_FAILED")?;
    let review = inspect_transfer_fixture(&input)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&review).map_err(|_| "PEER_REVIEW_SERIALIZE_FAILED")?
    );
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(code) => {
            eprintln!("{code}");
            std::process::ExitCode::FAILURE
        }
    }
}
