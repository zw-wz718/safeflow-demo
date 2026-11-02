//! Fictional CLI demonstration; not a registered or authenticated HTTP endpoint.
use safeflow_billing_source_reader::{query::*, spending::*, TrustedScope, DEMO_USER};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeSet, env, fs, process::ExitCode};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: u32,
    fictional: bool,
    production_registered: bool,
    source_id: String,
    samples: Vec<Sample>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sample {
    name: String,
    metadata: ReportMetadata,
    request: Request,
    steps: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    cursor: Option<String>,
    page: Option<Page>,
    error: Option<PageError>,
}
struct OfflineSource<'a> {
    id: &'a str,
    sample: &'a Sample,
}
impl PagedSource for OfflineSource<'_> {
    fn source_id(&self) -> &str {
        self.id
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        _: &Request,
        cursor: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        if scope.user_id() != DEMO_USER {
            return Err(PageError::ResourceUnavailable);
        }
        match self
            .sample
            .steps
            .iter()
            .find(|s| s.cursor.as_deref() == cursor)
        {
            Some(Step {
                page: Some(page),
                error: None,
                ..
            }) => Ok(page.clone()),
            Some(Step {
                page: None,
                error: Some(error),
                ..
            }) => Err(*error),
            _ => Err(PageError::Unavailable),
        }
    }
}
fn parse_args(args: Vec<String>) -> Result<(String, String), &'static str> {
    let mut args = args.into_iter();
    let mut path = None;
    let mut selected = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" if path.is_none() => {
                path = Some(args.next().ok_or("MISSING_FIXTURE_PATH")?)
            }
            "--case" if selected.is_none() => selected = Some(args.next().ok_or("MISSING_CASE")?),
            _ => return Err("UNSUPPORTED_ARGUMENT"),
        }
    }
    Ok((
        path.ok_or("FIXTURE_REQUIRED")?,
        selected.unwrap_or_else(|| "all".into()),
    ))
}
fn replay(input: &str, selected: &str) -> Result<Value, &'static str> {
    let fixture: Fixture = serde_json::from_str(input).map_err(|_| "INVALID_FIXTURE")?;
    let mut names = BTreeSet::new();
    let mut report_ids = BTreeSet::new();
    if fixture.schema_version != 1
        || !fixture.fictional
        || fixture.production_registered
        || !fixture.source_id.starts_with("fictional-")
        || fixture.samples.is_empty()
        || fixture.samples.iter().any(|sample| {
            let mut cursors = BTreeSet::new();
            sample.name.trim().is_empty()
                || sample.name == "all"
                || !names.insert(sample.name.clone())
                || !report_ids.insert(sample.metadata.report_id.clone())
                || sample.steps.is_empty()
                || sample.steps.iter().any(|step| {
                    step.page.is_some() == step.error.is_some()
                        || !cursors.insert(step.cursor.clone())
                })
        })
    {
        return Err("INVALID_FICTIONAL_FIXTURE");
    }
    if selected != "all" && !names.contains(selected) {
        return Err("UNKNOWN_CASE");
    }
    let scope = TrustedScope::from_verified_context(DEMO_USER, ["account_001", "account_002"])
        .map_err(|_| "INVALID_DEMO_CONTEXT")?;
    let mut cases = vec![];
    for sample in fixture
        .samples
        .iter()
        .filter(|s| selected == "all" || s.name == selected)
    {
        let source = OfflineSource {
            id: &fixture.source_id,
            sample,
        };
        let report = analyze_spending(
            &scope,
            &sample.request,
            &source,
            Limits::default(),
            &sample.metadata,
        )
        .map_err(|_| "INVALID_REPORT_METADATA")?;
        cases.push(json!({"name":sample.name,"report":report}));
    }
    Ok(
        json!({"mode":"OFFLINE_FICTIONAL_SPENDING_REPORT","fictional":true,"production_registered":false,
        "real_consumption_source_connected":false,"cases":cases}),
    )
}
fn run() -> Result<String, &'static str> {
    let (path, selected) = parse_args(env::args().skip(1).collect())?;
    let input = fs::read_to_string(path).map_err(|_| "INPUT_READ_FAILED")?;
    serde_json::to_string_pretty(&replay(&input, &selected)?).map_err(|_| "OUTPUT_FAILED")
}
fn main() -> ExitCode {
    match run() {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("basic-spending: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    const INPUT: &str = include_str!("../../../../../fixtures/billing/basic_spending.v1.json");
    #[test]
    fn fixture_and_selected_case_run_real_report() {
        let output = replay(INPUT, "mixed").unwrap();
        assert_eq!(output["cases"].as_array().unwrap().len(), 1);
        assert_eq!(
            output["cases"][0]["report"]["summary"]["gross_consumption"],
            "60.00"
        );
        assert_eq!(
            output["cases"][0]["report"]["refunds"][0]["transaction"]["amount"],
            "5.00"
        );
        assert_eq!(
            replay(INPUT, "all").unwrap()["cases"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
    }
    #[test]
    fn non_fictional_inputs_are_rejected() {
        for (key, value) in [
            ("fictional", json!(false)),
            ("production_registered", json!(true)),
            ("source_id", json!("real-source")),
            ("schema_version", json!(2)),
        ] {
            let mut raw: Value = serde_json::from_str(INPUT).unwrap();
            raw[key] = value;
            assert!(replay(&raw.to_string(), "all").is_err());
        }
    }
    #[test]
    fn duplicate_and_unknown_cases_are_rejected() {
        let mut raw: Value = serde_json::from_str(INPUT).unwrap();
        raw["samples"][1]["name"] = raw["samples"][0]["name"].clone();
        assert!(replay(&raw.to_string(), "all").is_err());
        assert!(replay(INPUT, "missing-case").is_err());
    }
    #[test]
    fn identity_and_unknown_fields_cannot_enter_fixture() {
        let mut raw: Value = serde_json::from_str(INPUT).unwrap();
        raw["user_id"] = json!("admin");
        assert!(replay(&raw.to_string(), "all").is_err());
        let args = vec![
            "--fixture".into(),
            "file".into(),
            "--user".into(),
            "admin".into(),
        ];
        assert!(parse_args(args).is_err());
        assert!(parse_args(vec!["--fixture".into()]).is_err());
        assert!(parse_args(vec![]).is_err());
    }
    #[test]
    fn malformed_step_and_ambiguous_cursors_are_rejected() {
        let mut raw: Value = serde_json::from_str(INPUT).unwrap();
        raw["samples"][0]["steps"][0]["error"] = json!("UNAVAILABLE");
        assert!(replay(&raw.to_string(), "all").is_err());
        let mut raw: Value = serde_json::from_str(INPUT).unwrap();
        raw["samples"][2]["steps"][1]["cursor"] = Value::Null;
        assert!(replay(&raw.to_string(), "all").is_err());
    }
}
