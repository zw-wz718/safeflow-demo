use safeflow_subscription_evidence::{filter_evidence, load_fixture};
use serde_json::json;

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
        return Err("USAGE: safeflow-subscription-evidence [mock-fixture.json]");
    }
    let path = args
        .first()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../../fixtures/subscription/SafeFlow_SUB-01_模拟流水与预期_v0.1.json")
        });
    let text = std::fs::read_to_string(path).map_err(|_| "FIXTURE_READ_FAILED")?;
    let fixtures = load_fixture(&text)?;
    let reports: Vec<_> = fixtures
        .samples
        .iter()
        .map(|sample| {
            json!({
                "sample_id": sample.sample_id,
                "report": filter_evidence(&sample.trusted_context_fixture, &sample.snapshot),
            })
        })
        .collect();
    let result = json!({
        "mode": "OFFLINE_MOCK_EVIDENCE_FILTER_ONLY",
        "recognition_executed": false,
        "rule_version": null,
        "sample_count": reports.len(),
        "reports": reports,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|_| "REPORT_SERIALIZE_FAILED")?
    );
    Ok(())
}
