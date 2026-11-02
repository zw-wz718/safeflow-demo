const ACTIONS = {
  create: { action: 'DRAFT_CREATED', result: 'DRAFT_CREATED' },
  update: { action: 'DRAFT_UPDATED', result: 'DRAFT_UPDATED' },
  cancel: { action: 'DRAFT_CANCELLED', result: 'DRAFT_CANCELLED' },
};

export function recordSimulatedDraft(recorder, input) {
  const transition = ACTIONS[input?.transition];
  if (!transition) throw new TypeError('transition must be create, update, or cancel');
  for (const field of ['event_id', 'task_id', 'operation_id', 'draft_ref']) {
    if (typeof input[field] !== 'string' || !input[field].trim()) throw new TypeError(`${field} is required`);
  }
  if (input.occurred_at != null) {
    throw new TypeError('the source draft sample has no business occurrence time; do not fill it from the local demo clock');
  }
  for (const field of ['business_operation_id', 'draft_amount']) {
    if (input[field] !== undefined && (typeof input[field] !== 'string' || !input[field].trim())) {
      throw new TypeError(`${field} must be a non-empty string when provided`);
    }
  }
  if (!Number.isInteger(input.draft_version) || input.draft_version < 1) throw new TypeError('draft_version must be a positive integer');
  const draftStatus = input.transition === 'cancel' ? 'CANCELLED' : 'READY_TO_CONFIRM';

  const record = recorder.recordAudit({
    event_id: input.event_id,
    kind: 'BUSINESS_EVENT',
    task_id: input.task_id,
    operation_id: input.operation_id,
    user_id: null,
    action: transition.action,
    timestamp: null,
    security_result: null,
    confirmation: null,
    auth_result: null,
    result: transition.result,
    tool_name: 'transfer_draft_demo',
    details: {
      demo_only: true,
      projection_source: 'LOCAL_AUDIT_DEMO',
      source_runtime_event_id_imported: false,
      source_runtime_event_id_status: 'RAW_RUNTIME_ID_IS_PROCESS_LOCAL; STABLE_PUBLIC_ID_PENDING',
      source_business_occurred_at: null,
      trusted_user_id: null,
      source_fields_not_provided_to_demo: ['raw_runtime_event_id', 'business_occurred_at', 'trusted_user_id'],
      demo_identifiers_generated_locally: true,
      draft_ref: input.draft_ref,
      ...(input.business_operation_id ? { business_operation_id: input.business_operation_id } : {}),
      draft_version: input.draft_version,
      ...(input.draft_amount ? { draft_amount: input.draft_amount } : {}),
      draft_status: draftStatus,
      note: '本地草案演示投影，不是 Rust Runtime 原始事件；原始 Runtime 编号未导入且稳定映射待确认，源业务发生时间和可信用户未提供，审计写入时间仅表示本机落库时间；不代表资金已提交或转账完成。',
    },
  });

  return {
    transition: input.transition,
    task_id: input.task_id,
    operation_id: input.operation_id,
    draft_ref: input.draft_ref,
    business_operation_id: input.business_operation_id ?? null,
    draft_version: input.draft_version,
    draft_amount: input.draft_amount ?? null,
    draft_status: draftStatus,
    record,
  };
}
