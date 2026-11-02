use super::*;
use safeflow_billing_source_reader::monthly_rules::plan_monthly;
struct Windows(Vec<(Request,safeflow_storage::SimulationCaptureReader)>);
impl PagedSource for Windows {
    fn source_id(&self)->&str { self.0[0].1.source_id() }
    fn read_page(&self,s:&TrustedScope,r:&Request,c:Option<&str>,v:Option<&str>)->Result<Page,PageError>{
        self.0.iter().find(|(request,_)|request==r).ok_or(PageError::ResourceUnavailable)?.1.read_page(s,r,c,v)
    }
}
impl Adapter {
    pub(super) async fn execute_monthly(&self,context:&ExecutionContext,scope:TrustedScope,request:Request,as_of:String,slot:tokio::sync::OwnedSemaphorePermit)->Result<ToolResult,ToolError>{
        if as_of!=safeflow_storage::TEAM_AS_OF { return Err(tool_error("INVALID_BILLING_WINDOW")); }
        let plan=plan_monthly(&as_of).map_err(tool_error)?.ok_or_else(||tool_error("INVALID_BILLING_WINDOW"))?;
        let pg=self.source.postgres.as_ref().ok_or_else(||tool_error("BILLING_SOURCE_UNAVAILABLE"))?;
        let mut captures=vec![]; let mut windows=vec![];
        for window in [plan.current,plan.previous,plan.historical] {
            let r=Request{account_ids:request.account_ids.clone(),window};
            let capture=pg.capture(&context.authenticated_user_id,&r).await.map_err(|_|tool_error("BILLING_SOURCE_UNAVAILABLE"))?;
            windows.push((r,capture.reader()));captures.push(capture);
        }
        let metadata=ReportMetadata{report_id:format!("billing_report_{}",context.operation_id),calculated_at:as_of.clone()};
        let revision=self.source.revision.clone();let accounts=request.account_ids.clone();
        let (monthly,evidence)=tokio::task::spawn_blocking(move ||{
            let _slot=slot;
            safeflow_storage::capture_monthly_billing_report(&scope,&accounts,&as_of,&Windows(windows),&metadata,&revision)
        }).await.map_err(|_|tool_error("BILLING_SOURCE_UNAVAILABLE"))?.map_err(|_|tool_error("INVALID_BILLING_METADATA"))?;
        // All three SHARE leases survive CPU work and the final real permission await.
        self.scope(&context.authenticated_user_id,&request).await.map_err(tool_error)?;
        for c in &captures {c.check_live().map_err(|_|tool_error("BILLING_SOURCE_EXPIRED"))?;}
        let current=monthly.current.as_ref().ok_or_else(||tool_error("INVALID_BILLING_METADATA"))?;
        let duplicates=detect_duplicates(&TrustedScope::from_verified_context(context.authenticated_user_id.as_str(),request.account_ids.iter().map(String::as_str)).map_err(|_|tool_error("INVALID_BILLING_SCOPE"))?,current);
        let mut result=ToolResult::new(name(),context.operation_id.clone(),result_ref(&context.operation_id),ToolResultStatus::Succeeded,
            json!({"adapter_contract":"LOCAL_BILLING_REPORT_ADAPTER_V2","source_revision":self.source.revision,"member_revision":MEMBER,"source_mode":"LOCAL_PG_THREE_CAPTURES_V1","production_source_connected":false,"report":current,"duplicate_candidates":duplicates,"monthly_report":monthly}));
        result.backend_evidence=evidence;Ok(result)
    }
}
