import { describe, expect, it } from 'vitest';
import { feedbackResultSchema } from './billing-feedback-client';

describe('candidate feedback domain audit response compatibility', () => {
  const original = {
    contract_version: 1,
    scope: 'BILLING_FEEDBACK_LOCAL_ONLY',
    task_id: 'original_task',
    operation_id: 'original_operation',
    task_state: 'COMPLETED',
    operation_state: 'SUCCEEDED',
    saved: {},
    replayed: true,
    lock_authorized: false,
    automatic_retry: false,
  };
  it('retains older response without claiming domain adoption', () => {
    expect(feedbackResultSchema.parse(original).domain_audit).toBeUndefined();
  });
  it.each(['NOT_ACTIVATED', 'LEGACY_WITHOUT_DOMAIN_EVENT', 'VERIFIED_V1'])(
    'reads explicit candidate status %s', (status) => {
      expect(feedbackResultSchema.parse({ ...original, domain_audit: status }).domain_audit).toBe(status);
    },
  );
  it.each([null, true, 'SENT', 'ACKED', { status: 'VERIFIED_V1' }])(
    'rejects malformed status %j', (status) => {
      expect(feedbackResultSchema.safeParse({ ...original, domain_audit: status }).success).toBe(false);
    },
  );
});
