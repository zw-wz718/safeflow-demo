//! Real project simulation assembly; public requests never carry authority/results.
use super::*;
use safeflow_contracts::ResourceId;
use serde_json::json;
impl ApiState {
    /// Explicit native opt-in after the unchanged bank assembly gates. The same
    /// checked read runtime uses the adopted bank source; no model write tools.
    pub async fn for_unified_bank_site_with_planner(
        source:PgUnifiedFictionalSource,config:ApiConfig,owner:UserId,
        controller:Zeroizing<String>,feedback:BillingFeedbackConfig,
        planner:safeflow_runtime::OpenAiGatewayConfig,
    )->Result<Self,Box<dyn std::error::Error>>{
        if config.agent_fixed_plan_enabled {return Err("bank planner cannot use fixed plans".into());}
        let gateway=Arc::new(safeflow_runtime::OpenAiGateway::new(planner)?);
        let mut state=Self::for_unified_bank_site(source,config,owner,controller,feedback).await?;
        let inner=Arc::get_mut(&mut state.0).ok_or("shared bank assembly")?;
        inner.llm_gateway=Some(gateway);
        inner.bank_planning_read_only=true;
        Ok(state)
    }
    /// Explicit native bank-chat assembly. Only the original checked draft and
    /// payment-intent runtime is exposed; all confirmation/auth gates remain.
    pub async fn for_unified_bank_site_with_conversation(
        source:PgUnifiedFictionalSource,config:ApiConfig,owner:UserId,
        controller:Zeroizing<String>,feedback:BillingFeedbackConfig,
        planner:safeflow_runtime::OpenAiGatewayConfig,
    )->Result<Self,Box<dyn std::error::Error>>{
        let mut state=Self::for_unified_bank_site_with_planner(source,config,owner,controller,feedback,planner).await?;
        Arc::get_mut(&mut state.0).ok_or("shared bank assembly")?.bank_planning_read_only=false;
        Ok(state)
    }
    /// Native-only opt-in, restricted HTTP login already explicitly deployed.
    pub async fn for_unified_bank_site(source:PgUnifiedFictionalSource,config:ApiConfig,owner:UserId,controller:Zeroizing<String>,feedback:BillingFeedbackConfig)->Result<Self,Box<dyn std::error::Error>>{
        let team=TeamConsumptionSource::open(&source,&owner).await?;
        let billing=crate::billing::team_billing_registration(Arc::new(source.clone()),&team)?;
        let demo_controller=if config.demo_code_display {Some(controller.clone())} else {None};
        let card=CardSafetyHost::for_isolated_native_host(controller.clone())?;
        let mut state=Self::bank_payment_assembly(source,config,owner,controller,vec![billing],Some(team),Some(feedback)).await?;
        Arc::get_mut(&mut state.0).ok_or("shared bank assembly")?.card_safety_host=Some(card);
        Arc::get_mut(&mut state.0).ok_or("shared bank assembly")?.demo_controller=demo_controller;
        Ok(state)
    }
}
pub(super) async fn overview(State(state):State<ApiState>,headers:HeaderMap,uri:Uri)->Result<Json<serde_json::Value>,ApiError>{
    let identity=state.identity(&headers)?;
    if uri.query().is_some(){return Err(ApiError::validation(StatusCode::BAD_REQUEST));}
    let mut values=vec![];
    for account in state.unified_accounts_for_backend(&identity.reference).await.map_err(ApiError::storage)?{
        let balance=state.unified_balance_for_backend(&identity.reference,&account.account_id).await.map_err(ApiError::storage)?;
        values.push(json!({"account":account,"balance":balance}));
    }
    identity.check(&state)?;
    Ok(Json(json!({"mode":"PROJECT_SIMULATION","owner_context":"CURRENT_SESSION","accounts":values,"real_bank":false,"real_llm":state.0.llm_gateway.is_some()})))
}
pub(super) async fn cards(State(state):State<ApiState>,headers:HeaderMap,uri:Uri)->Result<Json<serde_json::Value>,ApiError>{
    let identity=state.identity(&headers)?;
    if uri.query().is_some(){return Err(ApiError::validation(StatusCode::BAD_REQUEST));}
    let source=state.0.unified_source.as_ref().ok_or_else(ApiError::disabled)?;
    let mut items=vec![];
    for account in source.complete_accounts(&identity.binding.user_id).await.map_err(ApiError::storage)?{
        let catalog=source.authorized_catalog(&identity.binding,&account.account_id).await.map_err(ApiError::storage)?.ok_or_else(ApiError::unavailable)?;
        for card in catalog.cards {items.push(json!({"account_id":account.account_id,"account_display_name":account.display_name,"card_id":card.card_id,"last_four":card.last_four}));}
    }
    identity.check(&state)?;
    Ok(Json(json!({"mode":"PROJECT_SIMULATION","items":items})))
}
pub(super) async fn recovery(State(state):State<ApiState>,headers:HeaderMap,uri:Uri)->Result<Json<serde_json::Value>,ApiError>{
    let identity=state.identity(&headers)?;
    if uri.query().is_some(){return Err(ApiError::validation(StatusCode::BAD_REQUEST));}
    let result=tokio::time::timeout(state.0.request_timeout,
        state.0.repository.bank_recovery_for_backend(&identity.binding.user_id))
        .await.map_err(|_|ApiError::unavailable())?.map_err(ApiError::storage)?;
    identity.check(&state)?;
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Draft { nonce:String }
pub(super) async fn payment_draft(State(state):State<ApiState>,request:Request)->Result<Json<super::draft_lifecycle::LifecycleResponse>,ApiError>{
    let identity=state.identity(request.headers())?;
    let input:Draft=payment_preparation::input(request).await?;
    let nonce=BillingRecoveryNonce::parse(&input.nonce).map_err(|_|ApiError::validation(StatusCode::BAD_REQUEST))?;
    // Client persists this selector BEFORE POST. UNKNOWN recovers the same task,
    // never creates a replacement task/payment on refresh or network failure.
    let id=TaskId::parse(format!("bank_payment_{}",nonce.as_string())).map_err(|_|ApiError::validation(StatusCode::BAD_REQUEST))?;
    let result=tokio::time::timeout(state.0.request_timeout,async{
        let accounts=state.unified_accounts_for_backend(&identity.reference).await.map_err(ApiError::storage)?;
        if !accounts.iter().any(|a|a.account_id.as_str()=="account_001"){return Err(ApiError::forbidden());}
        super::draft_confirmation::candidates(&state,&identity,&id).await?;
        let view=super::draft_lifecycle::service(&state)?.with_payee_query_for_backend(ResourceId::parse("payee_008").unwrap(),super::draft_confirmation::FIXED_PAYEE_QUERY.into()).map_err(ApiError::storage)?
            .create(&identity.reference,&id,json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"100.00","currency":"CNY","remark":"","replaces":null}),7).await.map_err(ApiError::storage)?;
        super::draft_lifecycle::project(view)
    }).await.map_err(|_|ApiError::unavailable().with_task(&id))??;
    identity.check(&state)?;Ok(Json(result))
}
