use super::*;

impl State {
    fn registration_preview(
        &self,
        owner: &str,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<RegistrationPreviewMock> {
        self.guard("ledger", context, owner)?;
        self.ledger
            .registration_preview_mock(draft, context, now)
            .map_err(RuntimeRecoveryIssue::Business)
    }
    fn intent_preview(
        &self,
        owner: &str,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<IntentPreviewMock> {
        self.guard("ledger", context, owner)?;
        self.ledger
            .intent_preview_mock(draft, context, now)
            .map_err(RuntimeRecoveryIssue::Business)
    }
    fn settings_preview(
        &self,
        owner: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<crate::reminder_mock::SettingsPreviewMock> {
        self.guard("reminder", context, owner)?;
        let binding = self
            .settings
            .get(&context.operation_id)
            .ok_or(RuntimeRecoveryIssue::Reminder("SETTINGS_UNAVAILABLE"))?;
        if &binding.context != context {
            return Err(RuntimeRecoveryIssue::Reminder("PLAN_CHANGED"));
        }
        self.reminders
            .settings_preview_mock(&self.ledger, binding, now)
            .map_err(RuntimeRecoveryIssue::Reminder)
    }
}
impl SubscriptionRuntimeMock {
    pub fn registration_preview_mock(
        &self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<RegistrationPreviewMock> {
        self.state
            .registration_preview(&self.user, draft, context, now)
    }
    pub fn intent_preview_mock(
        &self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<IntentPreviewMock> {
        self.state.intent_preview(&self.user, draft, context, now)
    }
    pub fn settings_preview_mock(
        &self,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<crate::reminder_mock::SettingsPreviewMock> {
        self.state.settings_preview(&self.user, context, now)
    }
}
impl SubscriptionReadSnapshotMock {
    pub fn registration_preview_mock(
        &self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<RegistrationPreviewMock> {
        self.state
            .registration_preview(&self.owner, draft, context, now)
    }
    pub fn intent_preview_mock(
        &self,
        draft: &str,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<IntentPreviewMock> {
        self.state.intent_preview(&self.owner, draft, context, now)
    }
    pub fn settings_preview_mock(
        &self,
        context: &MockOperationContext,
        now: &str,
    ) -> Result<crate::reminder_mock::SettingsPreviewMock> {
        self.state.settings_preview(&self.owner, context, now)
    }
}
