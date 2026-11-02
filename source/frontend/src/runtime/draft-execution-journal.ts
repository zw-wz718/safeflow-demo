import { z } from 'zod';
import {
  draftExecutionReferenceSchema,
  type DraftExecutionReference,
} from './draft-execution-schema';

const journalKey = 'safeflow.runtime.draft.execution-attempts.v1';
const journalSchema = z
  .object({ version: z.literal(1), references: z.array(draftExecutionReferenceSchema) })
  .strict();
const memoryReferences = new Map<string, DraftExecutionReference>();

export function draftExecutionReferenceKey(reference: DraftExecutionReference) {
  return JSON.stringify([reference.task_id, reference.operation_id]);
}

export function readDraftExecutionJournal() {
  let available = true;
  try {
    const raw = sessionStorage.getItem(journalKey);
    if (raw !== null) {
      const parsed = journalSchema.safeParse(JSON.parse(raw));
      if (!parsed.success) available = false;
      else
        for (const reference of parsed.data.references)
          memoryReferences.set(draftExecutionReferenceKey(reference), reference);
    }
  } catch {
    available = false;
  }
  return { available, references: [...memoryReferences.values()] };
}

export function hasDraftExecutionAttempt(reference: DraftExecutionReference) {
  return readDraftExecutionJournal().references.some(
    (saved) => draftExecutionReferenceKey(saved) === draftExecutionReferenceKey(reference),
  );
}

export function hasDraftExecutionAttemptForTask(taskId: string) {
  return readDraftExecutionJournal().references.some((reference) => reference.task_id === taskId);
}

export function recordDraftExecutionAttempt(reference: DraftExecutionReference) {
  const parsed = draftExecutionReferenceSchema.safeParse(reference);
  if (!parsed.success) return false;
  const before = readDraftExecutionJournal();
  memoryReferences.set(draftExecutionReferenceKey(parsed.data), parsed.data);
  if (!before.available) return false;
  try {
    const references = [...memoryReferences.values()];
    sessionStorage.setItem(journalKey, JSON.stringify({ version: 1, references }));
    const stored = journalSchema.safeParse(
      JSON.parse(sessionStorage.getItem(journalKey) ?? 'null'),
    );
    if (!stored.success) return false;
    const storedKeys = new Set(stored.data.references.map(draftExecutionReferenceKey));
    return references.every((saved) => storedKeys.has(draftExecutionReferenceKey(saved)));
  } catch {
    return false;
  }
}
