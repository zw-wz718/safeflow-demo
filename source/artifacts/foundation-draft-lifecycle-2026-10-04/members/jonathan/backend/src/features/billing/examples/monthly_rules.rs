//! Offline demonstration only. No SQL, network, mutations, or identity from client input.
use chrono::DateTime;
use safeflow_billing_source_reader::{
    monthly_rules::analyze_monthly, query::*, spending::ReportMetadata, TrustedScope,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Bundle {
    source_id: String,
    as_of: String,
    users: Vec<User>,
}
#[derive(Deserialize)]
struct User {
    owner_user_id: String,
    request: Request,
    snapshot_id: String,
    pages: Vec<Step>,
}
#[derive(Deserialize)]
struct Step {
    page: Page,
}
struct Source<'a> {
    bundle: &'a Bundle,
    user: &'a User,
}
impl PagedSource for Source<'_> {
    fn source_id(&self) -> &str {
        &self.bundle.source_id
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        request: &Request,
        cursor: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        let parse = |s: &str| DateTime::parse_from_rfc3339(s).map_err(|_| PageError::Unavailable);
        if cursor.is_some()
            || scope.user_id() != self.user.owner_user_id
            || request.account_ids != self.user.request.account_ids
            || request.window.as_of != self.bundle.as_of
            || request.window.timezone != "Asia/Shanghai"
            || parse(&request.window.start_inclusive)?
                < parse(&self.user.request.window.start_inclusive)?
            || parse(&request.window.end_exclusive)?
                > parse(&self.user.request.window.end_exclusive)?
        {
            return Err(PageError::ResourceUnavailable);
        }
        let mut rows = vec![];
        // Validate the original whole fixture capture with the existing reader first.
        for step in &self.user.pages {
            rows.extend(step.page.items.clone());
        }
        Ok(Page {
            snapshot_id: self.user.snapshot_id.clone(),
            owner_user_id: self.user.owner_user_id.clone(),
            account_ids: request.account_ids.clone(),
            window: request.window.clone(),
            total_unique_rows: Some(rows.len()),
            items: rows,
            has_more: false,
            next_cursor: None,
            range_complete: true,
        })
    }
}
struct Original<'a>(&'a Bundle, &'a User);
impl PagedSource for Original<'_> {
    fn source_id(&self) -> &str {
        &self.0.source_id
    }
    fn read_page(
        &self,
        _: &TrustedScope,
        _: &Request,
        cursor: Option<&str>,
        _: Option<&str>,
    ) -> Result<Page, PageError> {
        let index = if cursor.is_none() {
            Some(0)
        } else {
            self.1
                .pages
                .iter()
                .position(|s| s.page.next_cursor.as_deref() == cursor)
                .map(|i| i + 1)
        };
        index
            .and_then(|i| self.1.pages.get(i))
            .map(|s| s.page.clone())
            .ok_or(PageError::Unavailable)
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::var_os("SAFEFLOW_BILLING_TEAM_DEMO_FIXTURE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../../fixtures/billing/team_demo_consumption.proposed.v1.json")
        });
    let bundle: Bundle = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let mut results = vec![];
    // Explicit known simulation scope; fixture metadata never grants authentication.
    for (owner, account) in [
        ("user_001", "account_001"),
        ("user_002", "account_002"),
        ("demo_user_li_v1", "demo_account_li_v1"),
    ] {
        let user = bundle
            .users
            .iter()
            .find(|u| u.owner_user_id == owner)
            .ok_or("Missing fixed demo user")?;
        let scope = TrustedScope::from_verified_context(owner, [account])?;
        if user.request.account_ids != [account]
            || !collect(
                &scope,
                &user.request,
                &Original(&bundle, user),
                Limits::default(),
            )
            .is_complete
        {
            return Err("Original fixture not complete for expected scope".into());
        }
        let report = analyze_monthly(
            &scope,
            &[account.into()],
            &bundle.as_of,
            &Source {
                bundle: &bundle,
                user,
            },
            Limits::default(),
            &ReportMetadata {
                report_id: format!("monthly-demo-{owner}"),
                calculated_at: bundle.as_of.clone(),
            },
        )?;
        results.push(json!({"owner_user_id":owner,"account_id":account,"rule_version":report.rule_version,"adoption_commit":report.adoption_commit,
            "production_registered":report.production_registered,"status":report.status,"plan":report.plan,"comparison":report.comparison,"anomalies":report.anomalies,"feedback_policy":report.feedback_policy}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"execution":"OFFLINE_FIXED_SIMULATION_NOT_JOINT_ACCEPTANCE","reports":results})
        )?
    );
    Ok(())
}
