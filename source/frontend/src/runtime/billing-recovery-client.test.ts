import { afterEach, expect, it, vi } from 'vitest';
import { agentSchema } from './agent-client';
import original from './billing-feedback-http-8ff.fixture.json';
import {
  recoveryPrepare,
  recoveryRead,
  recoveryAbandon,
  recoveryConfirm,
  type RecoveryCreated,
} from './billing-recovery-client';
const realm = '019d650e-e855-42ba-aea8-ef494930e559',
  nonce = 'a87eaf0c-d803-47c5-9935-8d6c26d48627';
const v = agentSchema.parse(original);
if (v.kind !== 'READ' || v.results?.[0].tool_name !== 'analyze_local_basic_spending')
  throw Error('original fixture');
const intent = {
  input_version: 1 as const,
  report_id: v.results[0].data.report.metadata.report_id,
  transaction: v.results[0].data.report.evidence![0].reference,
  choice: 'NOT_MINE' as const,
};
const envelope = {
  contract_version: 2 as const,
  scope: 'BILLING_FEEDBACK_LOCAL_ONLY' as const,
  recovery_realm: realm,
  nonce,
  lock_authorized: false as const,
  automatic_retry: false as const,
};
const prepared = (): RecoveryCreated => ({
  ...envelope,
  kind: 'CREATED',
  state: 'PREPARED_OPEN',
  revision: 1,
  task_id: 'task_protocol',
  operation_id: 'operation_protocol',
  parameters: { ...intent, original_digest: 'a'.repeat(64) },
  confirmation: {
    confirmation_ref: 'confirmation_protocol',
    status: 'PREPARED',
    expires_at: new Date(Date.now() + 300000).toISOString(),
  },
});
const read = () => ({
  ...envelope,
  kind: 'READ',
  state: 'PREPARED_OPEN',
  revision: 1,
  task_id: 'task_protocol',
  operation_id: 'operation_protocol',
  parameters: prepared().parameters,
  can_continue: false,
  confirmation_observation: {
    status: 'CONFIRMED',
    original_expires_at: new Date(Date.now() + 300000).toISOString(),
  },
  result_available: false,
  fence_revision: null,
});
const respond = (value: unknown, status = 200) =>
  vi.stubGlobal(
    'fetch',
    vi.fn().mockResolvedValue(new Response(JSON.stringify(value), { status })),
  );
afterEach(() => vi.unstubAllGlobals());
it('only exact CREATED bound to original identity yields a volatile ref', async () => {
  const p = prepared();
  respond(p, 201);
  expect(await recoveryPrepare(realm, nonce, intent)).toEqual(p);
  for (const patch of [
    { owner: 'forged' },
    { nonce: crypto.randomUUID() },
    { recovery_realm: crypto.randomUUID() },
    { automatic_retry: true },
    { kind: 'RECOVERY_ONLY' },
    { revision: 0 },
  ]) {
    respond({ ...p, ...patch }, 201);
    await expect(recoveryPrepare(realm, nonce, intent)).rejects.toThrow();
  }
});
it('read projections reject refs, inconsistent nulls, false fences and wrong original', async () => {
  respond(read());
  expect((await recoveryRead(realm, nonce)).can_continue).toBe(false);
  for (const patch of [
    { confirmation_ref: 'forged' },
    { can_continue: true },
    { task_id: null },
    { parameters: null },
    { fence_revision: 2 },
    { state: 'SAVED' },
    { nonce: crypto.randomUUID() },
    { state: 'unknown' },
  ]) {
    respond({ ...read(), ...patch });
    await expect(recoveryRead(realm, nonce)).rejects.toThrow();
  }
});
it('closing proof must match realm/nonce, paired originals and exact monotone fence', async () => {
  const proof = {
    ...envelope,
    kind: 'ABANDONED',
    state: 'ABANDONED',
    revision: 2,
    fence_revision: 2,
    task_id: null,
    operation_id: null,
  };
  respond(proof);
  expect((await recoveryAbandon(realm, nonce)).fence_revision).toBe(2);
  for (const patch of [
    { fence_revision: 1 },
    { nonce: crypto.randomUUID() },
    { operation_id: 'operation_other' },
    { parameters: {} },
    { state: 'SAVED' },
  ]) {
    respond({ ...proof, ...patch });
    await expect(recoveryAbandon(realm, nonce)).rejects.toThrow();
  }
});
it('confirmation cannot switch ref, operation or original expiry', async () => {
  const p = prepared();
  const reply = {
    ...p,
    kind: 'CONFIRMED',
    confirmation: { ...p.confirmation, status: 'CONFIRMED' },
  };
  delete (reply as Partial<typeof reply>).parameters;
  respond(reply);
  expect((await recoveryConfirm(p)).confirmation.status).toBe('CONFIRMED');
  respond({
    ...reply,
    confirmation: {
      ...reply.confirmation,
      expires_at: new Date(Date.now() + 600000).toISOString(),
    },
  });
  await expect(recoveryConfirm(p)).rejects.toThrow();
});
it.each([404, 409, 410, 429, 501, 503])(
  'v2 HTTP %s errors never fabricate a closing proof or retry',
  async (status) => {
    respond({ contract_version: 2, code: 'RECONCILE_REQUIRED' }, status);
    await expect(recoveryAbandon(realm, nonce)).rejects.toThrow();
    expect(vi.mocked(fetch)).toHaveBeenCalledTimes(1);
  },
);
