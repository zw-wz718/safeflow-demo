import { z } from 'zod';

const id = z.string().min(1);
const money = z.string().regex(/^(0|[1-9][0-9]*)\.[0-9]{2}$/);
const time = z.string().datetime({ offset: true });
const count = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER);
const reference = z.object({ source_id: id, account_id: id, transaction_id: id }).strict();
const transaction = z
  .object({
    transaction_id: id,
    account_id: id,
    operation_id: id.optional(),
    card_id: id.nullable(),
    amount: money,
    currency: z.literal('CNY'),
    direction: z.enum(['IN', 'OUT']),
    transaction_type: z.enum([
      'CONSUMPTION',
      'TRANSFER',
      'REFUND',
      'INCOME',
      'WITHDRAWAL',
      'OTHER',
    ]),
    status: z.enum(['COMPLETED', 'PROCESSING', 'FAILED', 'CANCELLED']),
    completed_at: time.nullable(),
    merchant_id: id.nullable(),
    category_code: id.optional(),
    original_tx_id: id.optional(),
    service_ref: id.optional(),
  })
  .strict();
const evidence = z.object({ reference, transaction }).strict();
const capability = z.object({ executed: z.literal(false), reason: id }).strict();
const billingV1 = z
  .object({
    adapter_contract: z.literal('LOCAL_BILLING_REPORT_ADAPTER_V1'),
    source_revision: id,
    source_mode: z.literal('LOCAL_BOUNDED_IN_PROCESS_V1'),
    production_source_connected: z.literal(false),
    report: z
      .object({
        contract: z.literal('LOCAL_BILLING_SPENDING_V1'),
        rule_version: z.literal('LOCAL_BASIC_SPENDING_2026_10_16_V1'),
        production_registered: z.literal(false),
        metadata: z.object({ report_id: id, calculated_at: time }).strict(),
        request: z
          .object({
            account_ids: z.array(id).min(1).max(16),
            window: z
              .object({
                as_of: time,
                start_inclusive: time,
                end_exclusive: time,
                timezone: z.literal('Asia/Shanghai'),
              })
              .strict(),
          })
          .strict(),
        read: z
          .object({
            source_id: id,
            state: z.enum([
              'COMPLETE',
              'DATA_INCOMPLETE',
              'LOOKUP_UNAVAILABLE',
              'RESOURCE_UNAVAILABLE',
              'INVALID_INPUT',
            ]),
            is_complete: z.boolean(),
            reason: id.nullable(),
            snapshot_id: id.nullable(),
            pages_read: count,
            duplicates_ignored: count,
            next_cursor: id.nullable(),
          })
          .strict(),
        calculation_status: z.enum(['READY', 'READ_BLOCKED', 'CALCULATION_UNAVAILABLE']),
        calculation_reason: id.nullable(),
        summary: z
          .object({
            count,
            gross_consumption: money,
            currency: z.literal('CNY'),
            categories: z.array(
              z
                .object({
                  category_code: id.nullable(),
                  display_name: id,
                  count,
                  total: money,
                  references: z.array(reference),
                })
                .strict(),
            ),
            top_five: z.array(evidence).max(5),
          })
          .strict()
          .nullable(),
        evidence: z.array(evidence).nullable(),
        refunds: z.array(evidence).nullable(),
        exclusions: z.array(z.object({ reference, reason: id }).strict()).nullable(),
        capabilities: z
          .object({ net_consumption: capability, comparison: capability, anomalies: capability })
          .strict(),
      })
      .strict(),
  })
  .strict();
const duplicateReport = z
  .object({
    contract: z.literal('LOCAL_DUPLICATE_CANDIDATES_V1'),
    rule_version: z.literal('LOCAL_SAME_MERCHANT_CNY_AMOUNT_0_600S_V1'),
    report_id: id,
    snapshot_id: id,
    source_id: id,
    production_registered: z.literal(false),
    state: z.enum(['COMPLETE', 'MERCHANT_EVIDENCE_INCOMPLETE', 'PAIR_LIMIT_EXCEEDED']),
    pairs: z
      .array(z.object({ first: reference, second: reference }).strict())
      .max(1000)
      .nullable(),
    missing_merchant: z.array(reference),
    risk_action_authorized: z.literal(false),
  })
  .strict();
const billingV2 = billingV1
  .extend({
    adapter_contract: z.literal('LOCAL_BILLING_REPORT_ADAPTER_V2'),
    member_revision: z.enum([
      'd822709a69dcc73c522adc711e23329758315c0c',
      '8ff8c6657d724d4f69e7a3d69266b12ec148e0ea',
    ]),
    duplicate_candidates: z.union([
      z.object({ Ok: duplicateReport }).strict(),
      z.object({ Err: z.enum(['REPORT_NOT_COMPLETE', 'RESOURCE_UNAVAILABLE']) }).strict(),
    ]),
  })
  .strict();
export const billingDataSchema = z.union([billingV1, billingV2]).superRefine((data, ctx) => {
  const r = data.report;
  if (
    (r.calculation_status === 'READY' &&
      (!r.summary || !r.read.is_complete || r.read.state !== 'COMPLETE')) ||
    r.read.is_complete !== (r.read.state === 'COMPLETE') ||
    (r.calculation_status === 'READ_BLOCKED' && r.summary !== null)
  )
    ctx.addIssue({ code: 'custom', message: 'Inconsistent report completeness' });
  if (data.adapter_contract !== 'LOCAL_BILLING_REPORT_ADAPTER_V2') return;
  const result = data.duplicate_candidates;
  if ('Err' in result) {
    if (result.Err === 'REPORT_NOT_COMPLETE' && r.read.is_complete)
      ctx.addIssue({ code: 'custom', message: 'Complete report cannot lack verified evidence' });
    return;
  }
  const d = result.Ok;
  const key = (ref: z.infer<typeof reference>) =>
    JSON.stringify([ref.source_id, ref.account_id, ref.transaction_id]);
  const rows = new Map((r.evidence ?? []).map((e) => [key(e.reference), e]));
  const refs = d.pairs?.flatMap((pair) => [pair.first, pair.second]) ?? [];
  const missing = d.missing_merchant.map(key);
  const missingKeys = new Set(missing);
  const pairs =
    d.pairs?.map((pair) => JSON.stringify([key(pair.first), key(pair.second)].sort())) ?? [];
  if (
    !r.read.is_complete ||
    r.evidence === null ||
    d.report_id !== r.metadata.report_id ||
    d.snapshot_id !== r.read.snapshot_id ||
    d.source_id !== r.read.source_id ||
    (d.state === 'PAIR_LIMIT_EXCEEDED') !== (d.pairs === null) ||
    (d.state === 'COMPLETE' && missing.length !== 0) ||
    (d.state === 'MERCHANT_EVIDENCE_INCOMPLETE' && missing.length === 0) ||
    missingKeys.size !== missing.length ||
    new Set(pairs).size !== pairs.length ||
    [...refs, ...d.missing_merchant].some(
      (ref) => ref.source_id !== d.source_id || !rows.has(key(ref)),
    ) ||
    d.pairs?.some(
      (pair) =>
        key(pair.first) === key(pair.second) ||
        [pair.first, pair.second].some(
          (ref) => rows.get(key(ref))?.transaction.merchant_id === null,
        ),
    ) ||
    (r.evidence ?? []).some(
      (e) =>
        e.reference.source_id !== d.source_id ||
        e.reference.account_id !== e.transaction.account_id ||
        e.reference.transaction_id !== e.transaction.transaction_id ||
        !r.request.account_ids.includes(e.reference.account_id),
    ) ||
    (r.evidence ?? []).some(
      (e) => (e.transaction.merchant_id === null) !== missingKeys.has(key(e.reference)),
    )
  )
    ctx.addIssue({
      code: 'custom',
      message: 'Inconsistent duplicate candidate binding or completeness',
    });
});
export type BillingData = z.infer<typeof billingDataSchema>;
