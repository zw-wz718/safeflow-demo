import { expect, it } from 'vitest';
import { subscriptionRecordId } from './subscription-display';
it('keeps all operation bits across refresh/order and ignores malformed saved labels', () => {
  const a = '12345678-1234-4234-8234-123456789abc',
    b = '12345678-1234-4234-8234-123456789abd';
  expect(subscriptionRecordId(a)).toBe(subscriptionRecordId(a, [{ nonce: b, recordId: 'broken' }]));
  expect(subscriptionRecordId(a)).not.toBe(subscriptionRecordId(b));
  expect(subscriptionRecordId('old-record')).not.toBe(subscriptionRecordId('oldrecord'));
  expect(subscriptionRecordId('')).toBe('记录信息待核对');
});
