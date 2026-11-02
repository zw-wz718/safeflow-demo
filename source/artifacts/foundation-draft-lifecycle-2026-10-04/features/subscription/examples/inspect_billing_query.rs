//! Fixed fictional identity; no production authentication or ledger override.
use safeflow_subscription_evidence::{
    billing_query_mock::load_case, recognition::recognize_mock, MockContext,
};
fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = (|| {
        if args.len() != 2 {
            return Err("USAGE: inspect_billing_query FILE CASE");
        }
        let input = std::fs::read_to_string(&args[0]).map_err(|_| "INPUT_READ_FAILED")?;
        let sample = load_case(
            &input,
            &args[1],
            MockContext {
                user_id: "user_demo".into(),
                requested_account_ids: vec!["account_001".into()],
            },
        )?;
        let report = recognize_mock(&sample.trusted_context_fixture, &sample.snapshot);
        serde_json::to_string_pretty(&report).map_err(|_| "OUTPUT_FAILED")
    })();
    match result {
        Ok(output) => {
            println!("{output}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(2)
        }
    }
}
