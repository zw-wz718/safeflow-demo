use safeflow_billing_source_reader::{query::*, TrustedScope, DEMO_USER};
use serde::Deserialize;
use serde_json::json;
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
        _request: &Request,
        cursor: Option<&str>,
        _snapshot: Option<&str>,
    ) -> Result<Page, PageError> {
        if scope.user_id() != DEMO_USER {
            return Err(PageError::ResourceUnavailable);
        }
        let step = self
            .sample
            .steps
            .iter()
            .find(|step| step.cursor.as_deref() == cursor)
            .ok_or(PageError::Unavailable)?;
        match (&step.page, step.error) {
            (Some(page), None) => Ok(page.clone()),
            (None, Some(error)) => Err(error),
            _ => Err(PageError::Unavailable),
        }
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mut path = None;
    let mut case = "all".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" => path = Some(args.next().ok_or("missing fixture path")?),
            "--case" => case = args.next().ok_or("missing case name")?,
            "--help" | "-h" => {
                println!("Offline fictional consumption replay: billing-consumption --fixture PATH [--case NAME|all]");
                return Ok(());
            }
            _ => return Err(format!("unsupported argument: {arg}").into()),
        }
    }
    let path = path.ok_or("--fixture is required")?;
    // No user/account identity option; fixtures cannot select authenticated scope.
    let fixture: Fixture = serde_json::from_str(&fs::read_to_string(path)?)?;
    let mut names = BTreeSet::new();
    if fixture.schema_version != 1
        || !fixture.fictional
        || fixture.production_registered
        || !fixture.source_id.starts_with("fictional-")
        || fixture.samples.is_empty()
        || fixture.samples.iter().any(|sample| {
            sample.name.trim().is_empty()
                || !names.insert(sample.name.clone())
                || sample.steps.is_empty()
                || sample
                    .steps
                    .iter()
                    .any(|step| step.page.is_some() == step.error.is_some())
        })
    {
        return Err("invalid fictional fixture metadata".into());
    }
    if case != "all" && !names.contains(&case) {
        return Err("unknown case name".into());
    }
    let scope = TrustedScope::from_verified_context(DEMO_USER, ["account_001", "account_002"])?;
    let mut outputs = vec![];
    for sample in fixture
        .samples
        .iter()
        .filter(|sample| case == "all" || sample.name == case)
    {
        let source = OfflineSource {
            id: &fixture.source_id,
            sample,
        };
        let report = collect(&scope, &sample.request, &source, Limits::default());
        let (snapshot, blocked) = match subscription_snapshot(&report, &scope, &sample.request) {
            Ok(value) => (Some(value), None),
            Err(reason) => (None, Some(reason)),
        };
        outputs.push(
            json!({"name":sample.name,"request":sample.request,"read":report,
            "subscription_snapshot":snapshot,"subscription_block_reason":blocked}),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"mode":"OFFLINE_FICTIONAL_BILLING_QUERY",
        "fictional":true,"production_registered":false,"formal_c_txn":false,
        "real_consumption_source_connected":false,"cases":outputs}))?
    );
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("billing-consumption: {error}");
            ExitCode::from(2)
        }
    }
}
