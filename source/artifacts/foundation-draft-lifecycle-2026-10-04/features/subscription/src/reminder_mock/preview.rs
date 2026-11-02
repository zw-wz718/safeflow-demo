use super::*;

#[derive(Clone, Debug, Serialize)]
pub struct SettingsPreviewMock {
    pub fictional: bool,
    pub subscription_id: String,
    pub before: Option<Schedule>,
    pub after: Schedule,
    pub changed_fields: Vec<&'static str>,
    pub expires_at: String,
    pub message: &'static str,
}
impl MemoryReminderMock {
    pub fn settings_preview_mock(
        &self,
        ledger: &MemoryConfirmationMock,
        binding: &ScheduleBinding,
        now: &str,
    ) -> Result<SettingsPreviewMock, &'static str> {
        let user = &binding.context.user_id;
        let target = owned_entry(ledger, user, &binding.target.subscription_id)?;
        let key = (user.clone(), binding.context.idempotency_key.clone());
        if self.completed.contains_key(&key) {
            return Err("OPERATION_CONFLICT");
        }
        if self.drafts.get(&key) != Some(binding) {
            return Err("PLAN_CHANGED");
        }
        let profile = (user.clone(), target.subscription_id.clone());
        if target != &binding.target
            || ledger
                .reminder_revision_mock(user)
                .map_err(|_| "LEDGER_NOT_READY")?
                != binding.ledger_revision
            || *self.versions.get(&profile).unwrap_or(&0) != binding.settings_revision
        {
            return Err("STALE_FACTS");
        }
        let parse = |s| DateTime::parse_from_rfc3339(s).map_err(|_| "TIME_INVALID");
        let time = parse(now)?;
        if time < parse(&binding.prepared_at)?
            || self.last_scan.get(user).is_some_and(|old| &time < old)
        {
            return Err("TIME_INVALID");
        }
        if time >= parse(&binding.expires_at)? {
            return Err("DRAFT_EXPIRED");
        }
        binding.schedule.validate()?;
        let before = self.profiles.get(&profile).cloned();
        let mut changed_fields = Vec::new();
        if let Some(old) = &before {
            if old.enabled != binding.schedule.enabled {
                changed_fields.push("enabled");
            }
            if old.days_before != binding.schedule.days_before {
                changed_fields.push("days_before");
            }
            if old.expected_date != binding.schedule.expected_date {
                changed_fields.push("expected_date");
            }
            if old.cycle != binding.schedule.cycle {
                changed_fields.push("cycle");
            }
            if old.anchor_date != binding.schedule.anchor_date {
                changed_fields.push("anchor_date");
            }
            if old.period_index != binding.schedule.period_index {
                changed_fields.push("period_index");
            }
        } else {
            changed_fields.push("initial_plan");
        }
        Ok(SettingsPreviewMock {fictional:true,subscription_id:target.subscription_id.clone(),before,after:binding.schedule.clone(),changed_fields,expires_at:binding.expires_at.clone(),
            message:"请核对提醒设置修改前后的差异；尚未保存，修改内容需重新确认。关闭普通续费提醒不撤回取消意图，也不清除未知尝试；保存设置不代表已发送通知。"})
    }
}
