import { z } from 'zod';

export const ownerIds = ['foundation', 'transfer', 'bills', 'cards', 'audit'] as const;
export const statuses = [
  'accepted',
  'partial',
  'in-progress',
  'pending-integration',
  'pending-review',
  'not-started',
  'unknown',
] as const;
export const statusLabels: Record<(typeof statuses)[number], string> = {
  accepted: '已验收',
  partial: '部分完成',
  'in-progress': '进行中',
  'pending-integration': '待对接',
  'pending-review': '待验收',
  'not-started': '未开始',
  unknown: '未收到进度',
};
export const officialTaskIds = [
  'BASE-01',
  'BASE-02',
  'BASE-03',
  'BASE-04',
  'BASE-05',
  'BASE-06',
  'COORD-01',
  'TRF-01',
  'TRF-02',
  'TRF-03',
  'TRF-04',
  'TRF-05',
  'TRF-06',
  'COORD-02',
  'COORD-03',
  'BILL-01',
  'BILL-02',
  'BILL-03',
  'BILL-04',
  'CARD-01',
  'CARD-02',
  'CARD-03',
  'SUB-01',
  'SUB-02',
  'SUB-03',
  'AUD-01',
  'AUD-02',
  'AUD-03',
  'AUD-04',
] as const;
const text = z.string().trim().min(1).max(2000);
const id = z.string().regex(/^[a-z][a-z0-9-]{1,80}$/);
const date = z
  .string()
  .regex(/^\d{4}-\d{2}-\d{2}$/)
  .refine((v) => {
    const parsed = new Date(`${v}T00:00:00Z`);
    return !Number.isNaN(parsed.valueOf()) && parsed.toISOString().slice(0, 10) === v;
  });
const ids = z.array(id).max(100);
const owner = z.enum(ownerIds);
const taskId = z.enum(officialTaskIds);
const source = z
  .object({
    id,
    title: text,
    kind: z.enum(['independent', 'review', 'member-report', 'implementation', 'plan', 'handoff']),
    path: text,
    date: date.nullable(),
    version: text.nullable(),
    scope: text,
    url: z
      .string()
      .max(2000)
      .refine((v) => {
        try {
          return ['https:', 'http:'].includes(new URL(v).protocol);
        } catch {
          return false;
        }
      })
      .optional(),
  })
  .strict();
const subtask = z
  .object({
    id,
    title: text,
    implemented: z.boolean(),
    accepted: z.boolean(),
    sourceIds: ids,
    scope: text,
  })
  .strict();
const task = z
  .object({
    id: taskId,
    ownerId: owner,
    title: text,
    priority: z.enum(['P0', 'P1', 'P0-Chain']),
    requirements: z.array(text).min(1).max(20),
    planSourceId: id,
    status: z.enum(statuses),
    implemented: z.boolean(),
    accepted: z.boolean(),
    scope: text,
    completed: z.array(text).max(30),
    gaps: z.array(text).max(30),
    next: text,
    version: text.nullable(),
    sourceIds: ids,
    subtasks: z.array(subtask).max(30),
  })
  .strict();
export const progressSchema = z
  .object({
    schemaVersion: z.literal(1),
    updatedAt: z.string().datetime({ offset: true }),
    current: z
      .object({
        headline: text,
        reviewedOn: date,
        sourceId: id,
        limits: z.array(text).min(1).max(12),
        tests: z
          .object({
            ordinary: z.number().int().nonnegative(),
            postgres: z.number().int().nonnegative(),
            doc: z.number().int().nonnegative(),
            independent: z.number().int().nonnegative(),
          })
          .strict(),
      })
      .strict(),
    members: z
      .array(z.object({ id: owner, name: text, role: text, stage: text, next: text }).strict())
      .length(5),
    sources: z.array(source).min(1).max(200),
    tasks: z.array(task).length(officialTaskIds.length),
    deliveries: z
      .array(
        z
          .object({
            id,
            ownerId: owner,
            receivedAt: date.nullable(),
            reviewedAt: date.nullable(),
            recordedAt: date,
            version: text,
            location: text,
            state: z.enum([
              'accepted',
              'member-report',
              'received',
              'pending-review',
              'changes-requested',
            ]),
            title: text,
            scope: text,
            declared: text,
            result: text,
            taskIds: z.array(taskId).min(1),
            sourceIds: ids.min(1),
          })
          .strict(),
      )
      .max(500),
    dependencies: z
      .array(
        z
          .object({
            id,
            title: text,
            ownerIds: z.array(owner).max(5),
            ownerNote: text,
            taskIds: z.array(taskId).min(1),
            material: text,
            impact: text,
            resumeWhen: text,
            sourceIds: ids.min(1),
          })
          .strict(),
      )
      .max(100),
  })
  .strict()
  .superRefine((data, ctx) => {
    const problem = (message: string) => ctx.addIssue({ code: z.ZodIssueCode.custom, message });
    for (const [label, records] of [
      ['成员', data.members],
      ['来源', data.sources],
      ['任务', data.tasks],
      ['交付', data.deliveries],
      ['依赖', data.dependencies],
    ] as const) {
      if (new Set(records.map((r) => r.id)).size !== records.length) problem(`${label}ID重复`);
    }
    if (ownerIds.some((key) => !data.members.some((m) => m.id === key))) problem('成员不完整');
    if (officialTaskIds.some((key) => !data.tasks.some((t) => t.id === key))) problem('任务不完整');
    const sources = new Map(data.sources.map((s) => [s.id, s]));
    const refs = (list: string[]) => {
      if (list.some((key) => !sources.has(key))) problem('未知证据引用');
    };
    if (sources.get(data.current.sourceId)?.kind !== 'independent') problem('当前验收需要独立来源');
    if (sources.get(data.current.sourceId)?.date !== data.current.reviewedOn)
      problem('当前验收日期与来源不一致');
    const checks = (item: { implemented: boolean; accepted: boolean; sourceIds: string[] }) => {
      refs(item.sourceIds);
      if (item.accepted && !item.implemented) problem('验收不能先于实现');
      if (
        item.implemented &&
        !item.sourceIds.some((key) => sources.has(key) && sources.get(key)?.kind !== 'plan')
      )
        problem('实现需要完成证据，计划不是完成证据');
      if (
        item.accepted &&
        !item.sourceIds.some(
          (key) => sources.get(key)?.kind === 'independent' && sources.get(key)?.date !== null,
        )
      )
        problem('验收需要有日期的独立证据');
    };
    for (const t of data.tasks) {
      checks(t);
      if (sources.get(t.planSourceId)?.kind !== 'plan') problem('任务需要PRD来源');
      if (t.accepted !== (t.status === 'accepted')) problem('任务状态与验收不一致');
      if (t.accepted && t.gaps.length) problem('仍有必要缺口的父任务不能验收完成');
      if (
        t.status === 'unknown' &&
        (t.implemented ||
          t.accepted ||
          t.completed.length ||
          t.subtasks.some((s) => s.implemented || s.accepted))
      )
        problem('未知任务不能声明完成');
      if (new Set(t.subtasks.map((s) => s.id)).size !== t.subtasks.length) problem('子项ID重复');
      for (const sub of t.subtasks) checks(sub);
      if (
        t.subtasks.length &&
        ((t.implemented && t.subtasks.some((s) => !s.implemented)) ||
          (t.accepted && t.subtasks.some((s) => !s.accepted)))
      )
        problem('父任务不能跳过必要子项');
    }
    for (const d of data.deliveries) {
      refs(d.sourceIds);
      if (
        d.state === 'accepted' &&
        (!d.reviewedAt || !d.sourceIds.some((key) => sources.get(key)?.kind === 'independent'))
      )
        problem('交付验收需要独立结果');
      if (d.state === 'member-report' && d.reviewedAt !== null) problem('自报不能填写验收日期');
      if (
        d.state === 'changes-requested' &&
        (!d.reviewedAt || !d.sourceIds.some((key) => sources.get(key)?.kind === 'review'))
      )
        problem('待修复需要独立检查结果');
    }
    for (const dependency of data.dependencies) refs(dependency.sourceIds);
  });

export type Progress = z.infer<typeof progressSchema>;
export type Task = Progress['tasks'][number];
export type Source = Progress['sources'][number];
export type OwnerId = (typeof ownerIds)[number];
export type Status = (typeof statuses)[number];

export function filterTasks(data: Progress, search: string, ownerId: string, status: string) {
  const query = search.trim().toLocaleLowerCase();
  return data.tasks.filter(
    (t) =>
      (ownerId === 'all' || t.ownerId === ownerId) &&
      (status === 'all' ||
        t.status === status ||
        (status === 'accepted' && t.subtasks.some((s) => s.accepted)) ||
        (status === 'pending-review' && t.subtasks.some((s) => s.implemented && !s.accepted))) &&
      [
        t.id,
        t.title,
        t.scope,
        t.version ?? '',
        ...t.requirements,
        ...t.completed,
        ...t.gaps,
        ...t.subtasks.map((s) => `${s.title} ${s.scope}`),
        data.members.find((m) => m.id === t.ownerId)?.name ?? '',
      ]
        .join(' ')
        .toLocaleLowerCase()
        .includes(query),
  );
}
export function memberCounts(tasks: Task[]) {
  const items = tasks.flatMap((t) => (t.subtasks.length ? t.subtasks : [t]));
  return {
    total: items.length,
    accepted: items.filter((t) => t.accepted).length,
    implemented: items.filter((t) => t.implemented).length,
  };
}
export function filterDependencies(
  data: Progress,
  search: string,
  ownerId: string,
  status: string,
) {
  const query = search.trim().toLocaleLowerCase();
  const applicable = new Set(filterTasks(data, '', 'all', status).map((task) => task.id));
  return data.dependencies.filter(
    (dependency) =>
      (ownerId === 'all' || dependency.ownerIds.some((id) => id === ownerId)) &&
      dependency.taskIds.some((id) => applicable.has(id)) &&
      [
        dependency.title,
        dependency.material,
        dependency.ownerNote,
        dependency.impact,
        dependency.resumeWhen,
        ...dependency.taskIds,
        ...dependency.ownerIds.map(
          (id) => data.members.find((member) => member.id === id)?.name ?? '',
        ),
      ]
        .join(' ')
        .toLocaleLowerCase()
        .includes(query),
  );
}
export function orderedDeliveries(data: Progress) {
  return data.deliveries
    .map((delivery, index) => ({
      delivery,
      index,
      date: delivery.reviewedAt ?? delivery.receivedAt ?? '',
    }))
    .sort((a, b) => {
      const byDate = b.date.localeCompare(a.date);
      // Undated records keep their ledger order; known same-day records show newest appended first.
      return byDate || (a.date ? b.index - a.index : a.index - b.index);
    })
    .map(({ delivery }) => delivery);
}
export const deliveryLabels: Record<Progress['deliveries'][number]['state'], string> = {
  accepted: '限定范围验收通过',
  'member-report': '组员报告完成',
  received: '已收到材料',
  'pending-review': '待验收',
  'changes-requested': '待修复',
};
export function deliveryDate(delivery: Progress['deliveries'][number]) {
  if (delivery.state === 'accepted') return { value: delivery.reviewedAt, label: '验收日期' };
  if (delivery.state === 'changes-requested')
    return { value: delivery.reviewedAt, label: '复核日期' };
  return {
    value: delivery.receivedAt,
    label: delivery.state === 'member-report' ? '报告收到日期' : '收到日期',
  };
}
