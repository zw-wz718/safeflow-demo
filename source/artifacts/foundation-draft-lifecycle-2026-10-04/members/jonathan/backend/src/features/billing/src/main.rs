use safeflow_billing_source_reader::*;
use serde_json::json;
use std::{env, fs, process::ExitCode};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mut fixture_path = None;
    let mut selected_case = "all".to_owned();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" => fixture_path = Some(args.next().ok_or("missing --fixture path")?),
            "--case" => selected_case = args.next().ok_or("missing --case value")?,
            "--help" | "-h" => {
                println!("Offline simulation only: billing-read --fixture PATH [--case all|success|failure|processing|lost-reply]");
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let cases = match selected_case.as_str() {
        "all" => vec![
            DemoCase::Success,
            DemoCase::Failure,
            DemoCase::Processing,
            DemoCase::LostReply,
        ],
        "success" => vec![DemoCase::Success],
        "failure" => vec![DemoCase::Failure],
        "processing" => vec![DemoCase::Processing],
        "lost-reply" => vec![DemoCase::LostReply],
        _ => {
            return Err(
                "invalid --case; expected all, success, failure, processing or lost-reply".into(),
            )
        }
    };
    let path =
        fixture_path.ok_or("--fixture PATH is required; only offline fixtures are supported")?;
    let fixture = TransferFixture::from_json(&fs::read_to_string(path)?)?;
    // Fixed simulation scope, independently supplied to the reader and source.
    // No --user/--account option and no identity read from fixture metadata.
    let scope = TrustedScope::from_verified_context(DEMO_USER, [DEMO_ACCOUNT])?;
    let mut outputs = Vec::new();
    for case in cases {
        let source = OfflineFixtureSource::new(fixture.clone(), case);
        let initial = source.initial_operation();
        let mut reader = BillingReader::default();
        let first = reader.read(&scope, &source)?;
        let repeated = reader.read(&scope, &source)?;
        let queried = refresh_operation(&scope, &source, &initial)?;
        outputs.push(json!({
            "case": case,
            "initial_operation": initial,
            "bill": first,
            "reread_added": repeated.added,
            "operation_after_original_id_query": queried,
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "mode": "OFFLINE_FIXED_FIXTURE_SIMULATION",
            "formal_c_txn": false,
            "subscription_limit": "未提供本人四个月消费、窗口/分页取全证明；不能判断订阅数量。",
            "cases": outputs,
        }))?
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("billing-read: {error}");
            ExitCode::from(2)
        }
    }
}
