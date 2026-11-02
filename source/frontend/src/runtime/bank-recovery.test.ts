import { describe, expect, it, vi, beforeEach } from 'vitest';
import fixture from './bank-recovery.fixture.json';
import { readPaymentTask, recoveryDirectory } from './bank-recovery';
import { request } from './client';
vi.mock('./client', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./client')>()),
  request: vi.fn(),
}));
const mock = vi.mocked(request);
beforeEach(() => mock.mockReset());
function setup(state: string, cancelled = false) {
  mock.mockResolvedValueOnce({
    ...fixture.draft,
    ...(cancelled ? { task_state: 'CANCELLED' } : {}),
  });
  mock.mockResolvedValueOnce([fixture.payment]);
  mock.mockResolvedValueOnce({
    ...fixture.funds,
    state,
    submitted: !['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(state),
  });
}
describe('server recovery only releases new work after every funds result is terminal', () => {
  it.each(['PROCESSING', 'RECONCILE_REQUIRED', 'SOURCE_CONFLICT', 'UNSUBMITTED'])(
    'keeps %s protected even when draft is cancelled',
    async (state) => {
      setup(state, true);
      expect((await readPaymentTask(fixture.draft.task_id)).terminal).toBe(false);
      expect(mock.mock.calls.every((c) => c[1] === 'GET')).toBe(true);
    },
  );
  it.each(['SUCCEEDED', 'FAILED'])('accepts verified %s terminal', async (state) => {
    setup(state);
    expect((await readPaymentTask(fixture.draft.task_id)).terminal).toBe(true);
  });
  it('fails closed on missing results or mismatched original ids', async () => {
    setup('PROCESSING');
    mock
      .mockReset()
      .mockResolvedValueOnce(fixture.draft)
      .mockResolvedValueOnce([fixture.payment])
      .mockRejectedValueOnce(Error('503'));
    await expect(readPaymentTask(fixture.draft.task_id)).rejects.toThrow('503');
    mock.mockReset().mockResolvedValueOnce({ ...fixture.draft, task_id: 'different' });
    await expect(readPaymentTask(fixture.draft.task_id)).rejects.toThrow('原号不匹配');
  });
  it('checks all preparations so a later terminal cannot hide an earlier unknown', async () => {
    mock
      .mockResolvedValueOnce(fixture.draft)
      .mockResolvedValueOnce([fixture.payment, fixture.payment])
      .mockResolvedValueOnce({ ...fixture.funds, state: 'RECONCILE_REQUIRED' })
      .mockResolvedValueOnce({ ...fixture.funds, state: 'FAILED', submitted: true });
    expect((await readPaymentTask(fixture.draft.task_id)).terminal).toBe(false);
    expect(mock).toHaveBeenCalledTimes(4);
  });
  it('rejects oversized directory instead of truncating protection', async () => {
    mock.mockResolvedValueOnce({
      context_ref: 'a'.repeat(64),
      payment_tasks: Array(257).fill('task'),
      subscription_nonces: [],
    });
    await expect(recoveryDirectory()).rejects.toThrow();
  });
});
