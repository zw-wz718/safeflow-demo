export type SubscriptionRequest = Record<string, unknown>;

/** A receipt absence/expired clock is insufficient. Only the server's exact,
 * positively invalidated original preparation can release the current selector. */
export function canLeaveSubscriptionPreparation(result: any, operation?: string) {
  const p = result?.preparation_recovery;
  return !!operation && p?.version === 1 && p.operation_id === operation &&
    p.may_start_new === true && p.state === 'NON_EXECUTABLE_PREPARATION' &&
    p.reason === 'RESTART_INVALIDATED' && p.original_preserved === true &&
    Number.isSafeInteger(p.original_writer_generation) && p.original_writer_generation > 0 &&
    Number.isSafeInteger(p.current_writer_generation) &&
    p.current_writer_generation > p.original_writer_generation &&
    result.original?.status === 'NEEDS_REVIEW' &&
    result.domain_audit?.status === 'UNEXECUTED_OR_NEEDS_REVIEW';
}
type Candidate = {
  selection_index: number;
  disposition: string;
  service_display_name?: string | null;
  charges?: Array<{ completed_at: string; amount_minor_units: number; currency: string }>;
};
export type SubscriptionCapture = {
  capture_ref: string;
  preview?: { recognition?: { groups?: Candidate[] } };
};
/** Display evidence only. Ambiguous or changed evidence requires a new user selection. */
export function candidateIdentity(group?: Candidate) {
  if (!group?.charges?.length) throw Error('未取得完整候选依据，请重新识别');
  return JSON.stringify([
    group.service_display_name ?? null,
    group.charges
      .map((c) => [c.completed_at, c.amount_minor_units, c.currency])
      .sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b))),
  ]);
}
export function rebindSubscriptionRequest(
  pending: { request: SubscriptionRequest; candidate?: string },
  fresh: SubscriptionCapture,
) {
  if (pending.request.kind !== 'REGISTRATION' || !pending.candidate)
    throw Error('请重新读取并选择订阅，旧选择不能复用');
  const matches =
    fresh.preview?.recognition?.groups?.filter((g) => candidateIdentity(g) === pending.candidate) ??
    [];
  if (matches.length !== 1 || matches[0].disposition !== 'NOT_IN_LEDGER')
    throw Error('订阅候选已变化或无法唯一对应，请重新核对');
  return {
    ...pending.request,
    capture_ref: fresh.capture_ref,
    candidate_index: matches[0].selection_index,
  };
}
