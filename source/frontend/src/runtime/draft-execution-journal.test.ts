import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const key = 'safeflow.runtime.draft.execution-attempts.v1';
const original = { task_id: 'task_1', operation_id: 'operation_1' };
const second = { task_id: 'task_2', operation_id: 'operation_2' };
let values: Map<string, string>;
let storage: { getItem: ReturnType<typeof vi.fn>; setItem: ReturnType<typeof vi.fn> };

beforeEach(() => {
  vi.resetModules();
  values = new Map();
  storage = {
    getItem: vi.fn((name: string) => values.get(name) ?? null),
    setItem: vi.fn((name: string, value: string) => values.set(name, value)),
  };
  vi.stubGlobal('sessionStorage', storage);
});
afterEach(() => vi.unstubAllGlobals());

describe('draft execution attempt references', () => {
  it('retains all attempted original pairs across task switches and reloads', async () => {
    let journal = await import('./draft-execution-journal');
    expect(journal.recordDraftExecutionAttempt(original)).toBe(true);
    expect(journal.recordDraftExecutionAttempt(second)).toBe(true);
    expect(journal.recordDraftExecutionAttempt(original)).toBe(true);
    vi.resetModules();
    journal = await import('./draft-execution-journal');
    expect(journal.readDraftExecutionJournal().references).toEqual([original, second]);
    expect(journal.hasDraftExecutionAttempt(original)).toBe(true);
    expect(journal.hasDraftExecutionAttemptForTask(original.task_id)).toBe(true);
  });
  it('writes only the two original references and schema version', async () => {
    const journal = await import('./draft-execution-journal');
    journal.recordDraftExecutionAttempt(original);
    expect(JSON.parse(values.get(key)!)).toEqual({ version: 1, references: [original] });
  });
  it('blocks submission when storage throws or does not actually retain the pair', async () => {
    const journal = await import('./draft-execution-journal');
    storage.setItem.mockImplementation(() => {
      throw new Error('Unavailable');
    });
    expect(journal.recordDraftExecutionAttempt(original)).toBe(false);
    expect(journal.hasDraftExecutionAttempt(original)).toBe(true);
    storage.setItem.mockImplementation(() => undefined);
    expect(journal.recordDraftExecutionAttempt(second)).toBe(false);
  });
  it('does not erase earlier attempts when browser storage is cleared within the page', async () => {
    const journal = await import('./draft-execution-journal');
    journal.recordDraftExecutionAttempt(original);
    values.clear();
    expect(journal.hasDraftExecutionAttempt(original)).toBe(true);
    expect(journal.recordDraftExecutionAttempt(second)).toBe(true);
    expect(JSON.parse(values.get(key)!).references).toEqual([original, second]);
  });
  it.each([
    'invalid-json',
    '{"version":1,"references":[{"task_id":"t","operation_id":"o","confirmation_ref":"secret"}]}',
  ])('fails closed on corrupt or secret-bearing records: %s', async (raw) => {
    values.set(key, raw);
    const journal = await import('./draft-execution-journal');
    expect(journal.readDraftExecutionJournal().available).toBe(false);
    expect(journal.recordDraftExecutionAttempt(original)).toBe(false);
    expect(values.get(key)).toBe(raw);
  });
  it('refuses extra identity fields instead of storing them', async () => {
    const journal = await import('./draft-execution-journal');
    expect(
      journal.recordDraftExecutionAttempt({
        ...original,
        confirmation_ref: 'secret',
      } as typeof original),
    ).toBe(false);
    expect(storage.setItem).not.toHaveBeenCalled();
  });
});
