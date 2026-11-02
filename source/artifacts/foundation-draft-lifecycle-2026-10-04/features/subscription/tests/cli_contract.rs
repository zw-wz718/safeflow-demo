use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const INPUT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/SafeFlow_SUB-01_模拟流水与预期_v0.1.json"
));
const SAVED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/SafeFlow_SUB-01_证据筛选实测_v0.1.json"
));
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

struct InputFile(PathBuf);

impl InputFile {
    fn new(bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "SafeFlow 订阅模拟 {}-{}.json",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, bytes).unwrap();
        Self(path)
    }
}

impl Drop for InputFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn cli(path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_safeflow-subscription-evidence"))
        .current_dir(std::env::temp_dir())
        .arg(path)
        .output()
        .unwrap()
}

fn failure(output: Output, message: &str) {
    assert!(!output.status.success());
    // A caller must never receive partial success JSON on a file-level failure.
    assert!(output.stdout.is_empty());
    assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), message);
}

#[test]
fn explicit_unicode_path_from_another_directory_matches_saved_replay() {
    let input = InputFile::new(INPUT.as_bytes());
    let output = cli(&input.0);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    let expected: Value = serde_json::from_str(SAVED).unwrap();
    // Object key order and JSON whitespace do not affect this comparison.
    assert_eq!(actual, expected);
}

#[test]
fn malformed_json_and_invalid_utf8_produce_only_fixed_errors() {
    for bytes in [b"{PRIVATE-FIXTURE-CONTENT".as_slice(), &[0xff, 0xfe]] {
        let input = InputFile::new(bytes);
        let error = if bytes[0] == 0xff {
            "FIXTURE_READ_FAILED"
        } else {
            "FIXTURE_PARSE_FAILED"
        };
        failure(cli(&input.0), error);
    }
}

#[test]
fn invalid_metadata_after_valid_samples_never_emits_partial_reports() {
    let mut value: Value = serde_json::from_str(INPUT).unwrap();
    value["samples"][17]["sample_id"] = value["samples"][0]["sample_id"].clone();
    let input = InputFile::new(value.to_string().as_bytes());
    failure(cli(&input.0), "FIXTURE_METADATA_INVALID");
}

#[test]
fn unknown_enum_in_last_sample_never_emits_partial_reports() {
    let mut value: Value = serde_json::from_str(INPUT).unwrap();
    value["samples"][17]["snapshot"]["transactions"][0]["status"] = "PRIVATE-UNKNOWN-STATUS".into();
    let input = InputFile::new(value.to_string().as_bytes());
    failure(cli(&input.0), "FIXTURE_PARSE_FAILED");
}

#[test]
fn excess_arguments_are_rejected_before_reading_a_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_safeflow-subscription-evidence"))
        .args(["PRIVATE-FIRST-PATH.json", "PRIVATE-SECOND-PATH.json"])
        .output()
        .unwrap();
    failure(
        output,
        "USAGE: safeflow-subscription-evidence [mock-fixture.json]",
    );
}
