//! File-only proposal comparison. Never invokes a bank, Runtime execution or storage.
use safeflow_runtime::review_mock_transfer_receipt_proposal;
use serde_json::{json, Value};
use std::{error::Error, fs::File, io::Read, path::Path};

fn bounded_read(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut raw = Vec::new();
    File::open(path)?.take(65537).read_to_end(&mut raw)?;
    if raw.len() > 65536 {
        return Err("explicit proposal file exceeds 64 KiB".into());
    }
    Ok(raw)
}
fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let reports = if args.is_empty() {
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/received_transfer_2a72b5d7.json"))?;
        let expected = serde_json::to_vec(&fixture["request"])?;
        let mut reports = vec![];
        for case in ["success", "failure", "processing", "lost_reply"] {
            let received = serde_json::to_vec(&fixture[case])?;
            let review = review_mock_transfer_receipt_proposal(&expected, &received)?;
            reports.push(json!({"case":case,"review":review.summary()}));
        }
        reports
    } else if args.len() == 4 && args[0] == "--request" && args[2] == "--receipt" {
        let expected = bounded_read(Path::new(&args[1]))?;
        let received = bounded_read(Path::new(&args[3]))?;
        let review = review_mock_transfer_receipt_proposal(&expected, &received)?;
        vec![json!({"case":"EXPLICIT_LOCAL_FILES","review":review.summary()})]
    } else {
        return Err("usage: no args, or --request <request.json> --receipt <receipt.json>".into());
    };
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"scope":"PROPOSAL_SCHEMA_COMPARISON_ONLY_NO_BUSINESS_EXECUTION","reviews":reports})
        )?
    );
    Ok(())
}
