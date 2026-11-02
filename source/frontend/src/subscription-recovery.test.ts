import { describe, expect, it } from 'vitest';
import { rebindSubscriptionRequest, candidateIdentity, canLeaveSubscriptionPreparation } from './subscription-recovery';
const a = {
  selection_index: 2,
  disposition: 'NOT_IN_LEDGER',
  service_display_name: '演示服务',
  charges: [{ completed_at: '2026-10-01T00:00:00Z', amount_minor_units: 990, currency: 'CNY' }],
};
const pending = {
  request: { kind: 'REGISTRATION', capture_ref: 'old', candidate_index: 2 },
  candidate: candidateIdentity(a),
};
const capture = (...groups: (typeof a)[]) => ({
  capture_ref: 'new',
  preview: { recognition: { groups } },
});
describe('subscription retry preserves observed candidate, not its old position', () => {
  it('requires positive exact restart invalidation; expiry, absent receipt, different operation and generation cannot unlock',()=>{
    const v={original:{status:'NEEDS_REVIEW'},domain_audit:{status:'UNEXECUTED_OR_NEEDS_REVIEW'},preparation_recovery:{version:1,operation_id:'op_a',may_start_new:true,state:'NON_EXECUTABLE_PREPARATION',reason:'RESTART_INVALIDATED',original_preserved:true,original_writer_generation:6,current_writer_generation:9}};
    expect(canLeaveSubscriptionPreparation(v,'op_a')).toBe(true);
    expect(canLeaveSubscriptionPreparation(v,'op_b')).toBe(false);
    for(const patch of [{reason:'EXPIRED'},{may_start_new:false},{current_writer_generation:6},{original_writer_generation:0},{original_preserved:false}])
      expect(canLeaveSubscriptionPreparation({...v,preparation_recovery:{...v.preparation_recovery,...patch}},'op_a')).toBe(false);
    expect(canLeaveSubscriptionPreparation({...v,original:{status:'UNKNOWN'}},'op_a')).toBe(false);
    expect(canLeaveSubscriptionPreparation({...v,domain_audit:{status:'VERIFIED_CANDIDATE_V1'}},'op_a')).toBe(false);
    expect(canLeaveSubscriptionPreparation({original:{status:'NEEDS_REVIEW'}},'op_a')).toBe(false);
  });
  it('rebinds unique unchanged evidence after reorder', () => {
    expect(rebindSubscriptionRequest(pending, capture({ ...a, selection_index: 0 }))).toEqual({
      ...pending.request,
      capture_ref: 'new',
      candidate_index: 0,
    });
  });
  it('rejects another candidate now occupying the old index', () => {
    expect(() =>
      rebindSubscriptionRequest(
        pending,
        capture({ ...a, charges: [{ ...a.charges[0], amount_minor_units: 1000 }] }),
      ),
    ).toThrow();
  });
  it('rejects ambiguous evidence and already managed candidates', () => {
    expect(() =>
      rebindSubscriptionRequest(pending, capture(a, { ...a, selection_index: 1 })),
    ).toThrow();
    expect(() =>
      rebindSubscriptionRequest(pending, capture({ ...a, disposition: 'ALREADY_IN_LEDGER' })),
    ).toThrow();
  });
  it('never attaches capture to a different action or guesses missing old evidence', () => {
    expect(() =>
      rebindSubscriptionRequest({ request: { kind: 'REMINDER_SETTINGS' } }, capture(a)),
    ).toThrow();
    expect(() => rebindSubscriptionRequest({ request: pending.request }, capture(a))).toThrow();
  });
});
