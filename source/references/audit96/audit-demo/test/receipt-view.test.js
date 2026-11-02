import assert from 'node:assert/strict';
import test from 'node:test';
import { createParticipantReceipt } from '../src/receipt-view.js';

test('participant receipts are limited to explicitly simulated transfer events', () => {
  const sourceEvent = {
    action: 'TRANSFER_SUCCEEDED', operation_id: 'op-source', result: 'SUCCEEDED',
    timestamp: '2026-10-19T06:00:00.000Z', details: { amount: 'CNY 200.00' },
  };
  assert.equal(createParticipantReceipt([sourceEvent], 'payer'), null);
  assert.equal(createParticipantReceipt([{ ...sourceEvent, details: {} }], 'recipient'), null);

  const demo = { ...sourceEvent, operation_id: 'op-demo', details: { amount: 'CNY 200.00', simulated: true } };
  const receipt = createParticipantReceipt([sourceEvent, demo], 'payer');
  assert.equal(receipt.simulated, true);
  assert.equal(receipt.status, 'SUCCEEDED');
  assert.equal(receipt.amount, 'CNY 200.00');
  assert.equal(createParticipantReceipt([{ ...demo, operation_id: null }], 'payer'), null);
});
