import { progressSchema, type Progress } from './schema';

export type SyncState = {
  data: Progress | null;
  status: 'loading' | 'synced' | 'unsynced';
  lastSuccessAt: string | null;
};
export const initialSyncState: SyncState = { data: null, status: 'loading', lastSuccessAt: null };
export function syncSucceeded(data: unknown, now = new Date().toISOString()): SyncState {
  return { data: progressSchema.parse(data), status: 'synced', lastSuccessAt: now };
}
export function syncFailed(previous: SyncState): SyncState {
  return { ...previous, status: 'unsynced' };
}
export async function fetchProgress(signal: AbortSignal) {
  const response = await fetch('/api/progress', { signal, cache: 'no-store' });
  if (!response.ok) throw new Error('PROGRESS_UNAVAILABLE');
  return progressSchema.parse(await response.json());
}
export function exportProgress(data: Progress) {
  const blob = new Blob([`${JSON.stringify(data, null, 2)}\n`], {
    type: 'application/json;charset=utf-8',
  });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = `SafeFlow_项目进度_${data.updatedAt.slice(0, 10)}.json`;
  anchor.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
