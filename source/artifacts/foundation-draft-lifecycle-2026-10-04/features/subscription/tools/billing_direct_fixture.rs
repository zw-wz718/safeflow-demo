// Test-only source using Jonathan's unchanged explicit fictional source pages.
use safeflow_billing_source_reader::{query as billing, TrustedScope};
use serde_json::{json, Value};
pub struct FixtureSource {
    pub source: String,
    pub steps: Vec<Value>,
}
impl billing::PagedSource for FixtureSource {
    fn source_id(&self) -> &str {
        &self.source
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        _: &billing::Request,
        cursor: Option<&str>,
        _: Option<&str>,
    ) -> Result<billing::Page, billing::PageError> {
        let step = self
            .steps
            .iter()
            .find(|s| s["cursor"] == json!(cursor))
            .ok_or(billing::PageError::Unavailable)?;
        if let Some(error) = step.get("error").filter(|e| !e.is_null()) {
            return Err(serde_json::from_value(error.clone())
                .map_err(|_| billing::PageError::Unavailable)?);
        }
        serde_json::from_value(step["page"].clone()).map_err(|_| billing::PageError::Unavailable)
    }
}
pub fn fixture(name: &str) -> Result<(TrustedScope, billing::Request, FixtureSource), String> {
    let v: Value = serde_json::from_str(include_str!(env!(
        "SAFEFLOW_SUBSCRIPTION_DIRECT_BILLING_FIXTURE"
    )))
    .map_err(|_| "SOURCE_FIXTURE_INVALID")?;
    if v["fictional"] != true || v["production_registered"] != false {
        return Err("SOURCE_NOT_FICTIONAL".into());
    }
    let sample = v["samples"]
        .as_array()
        .ok_or("SAMPLES_MISSING")?
        .iter()
        .find(|s| s["name"] == name)
        .ok_or("SAMPLE_MISSING")?;
    let request: billing::Request =
        serde_json::from_value(sample["request"].clone()).map_err(|_| "REQUEST_INVALID")?;
    let scope = TrustedScope::from_verified_context(
        "user_demo",
        request.account_ids.iter().map(String::as_str),
    )
    .map_err(|_| "SCOPE_INVALID")?;
    Ok((
        scope,
        request,
        FixtureSource {
            source: v["source_id"].as_str().ok_or("SOURCE_ID_MISSING")?.into(),
            steps: sample["steps"].as_array().ok_or("STEPS_MISSING")?.clone(),
        },
    ))
}
pub struct FailingSource;
impl billing::PagedSource for FailingSource {
    fn source_id(&self) -> &str {
        "fictional-unavailable-source"
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        _: &billing::Request,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<billing::Page, billing::PageError> {
        Err(billing::PageError::Unavailable)
    }
}
