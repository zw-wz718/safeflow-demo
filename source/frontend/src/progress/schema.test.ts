import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import {
  filterTasks,
  filterDependencies,
  deliveryDate,
  deliveryLabels,
  memberCounts,
  officialTaskIds,
  orderedDeliveries,
  progressSchema,
} from './schema';
import { initialSyncState, syncFailed, syncSucceeded } from './client';
import type { Progress } from './schema';

const load = () =>
  JSON.parse(
    readFileSync(new URL('../../../docs/progress/project-progress.json', import.meta.url), 'utf8'),
  );
describe('delivery display ordering', () => {
  function fixture(entries: Partial<Progress['deliveries'][number]>[]) {
    const data = progressSchema.parse(load());
    const template = data.deliveries[0];
    data.deliveries = entries.map((entry) => ({
      ...template,
      state: 'received',
      receivedAt: null,
      reviewedAt: null,
      ...entry,
    }));
    return data;
  }
  it('shows later appended records first on the same known date', () => {
    const data = fixture([
      { id: 'pending', receivedAt: '2026-10-06' },
      { id: 'accepted', state: 'accepted', reviewedAt: '2026-10-06' },
    ]);
    expect(orderedDeliveries(data).map((d) => d.id)).toEqual(['accepted', 'pending']);
  });
  it('prioritizes the known review or receipt date over append order across days', () => {
    const data = fixture([
      { id: 'newest', receivedAt: '2026-10-06' },
      { id: 'reviewed', receivedAt: '2026-10-01', reviewedAt: '2026-10-05' },
      { id: 'last-appended', receivedAt: '2026-10-04' },
    ]);
    expect(orderedDeliveries(data).map((d) => d.id)).toEqual([
      'newest',
      'reviewed',
      'last-appended',
    ]);
  });
  it('keeps unknown dates last in ledger order without using recordedAt', () => {
    const data = fixture([
      { id: 'unknown-first', recordedAt: '2026-10-08' },
      { id: 'known', receivedAt: '2026-10-04', recordedAt: '2026-10-04' },
      { id: 'unknown-last', recordedAt: '2026-10-09' },
    ]);
    const result = orderedDeliveries(data);
    expect(result.map((d) => d.id)).toEqual(['known', 'unknown-first', 'unknown-last']);
    for (const delivery of result.slice(1)) {
      expect(deliveryDate(delivery)).toEqual({ value: null, label: '收到日期' });
    }
  });
  it('sorts a separate display array without changing the input ledger or records', () => {
    const data = fixture([
      { id: 'first', receivedAt: '2026-10-06' },
      { id: 'second', receivedAt: '2026-10-06' },
    ]);
    const before = structuredClone(data);
    Object.freeze(data.deliveries);
    data.deliveries.forEach(Object.freeze);
    const result = orderedDeliveries(data);
    expect(result).not.toBe(data.deliveries);
    expect(result.map((d) => d.id)).toEqual(['second', 'first']);
    expect(data).toEqual(before);
  });
});
describe('persistent project progress semantics', () => {
  it('covers actual PRD tasks and five owners without overstating parent completion', () => {
    const data = progressSchema.parse(load());
    expect(data.tasks.map((t) => t.id).sort()).toEqual([...officialTaskIds].sort());
    expect(data.tasks.filter((t) => t.ownerId === 'foundation').every((t) => !t.accepted)).toBe(
      true,
    );
    expect(data.tasks.find((t) => t.id === 'CARD-01')?.accepted).toBe(false);
    expect(
      data.tasks.find((t) => t.id === 'CARD-01')?.subtasks.find((s) => s.id === 'readonly')
        ?.accepted,
    ).toBe(true);
    expect(data.sources.find((source) => source.id === data.current.sourceId)).toMatchObject({
      kind: 'independent',
      date: data.current.reviewedOn,
    });
    expect(
      data.deliveries.some(
        (delivery) =>
          delivery.state === 'accepted' &&
          delivery.reviewedAt === data.current.reviewedOn &&
          delivery.sourceIds.includes(data.current.sourceId),
      ),
    ).toBe(true);
  });
  it('accepts only the task-query child and appends its review without replacing the implementation history', () => {
    const data = progressSchema.parse(load());
    const task = data.tasks.find((t) => t.id === 'BASE-05')!;
    expect(task).toMatchObject({ status: 'partial', implemented: false, accepted: false });
    expect(task.subtasks.find((s) => s.id === 'task-query')).toMatchObject({
      implemented: true,
      accepted: true,
      sourceIds: expect.arrayContaining(['task-query-review']),
    });
    expect(
      task.subtasks
        .filter((s) => ['http', 'original-operation'].includes(s.id))
        .every((s) => !s.implemented && !s.accepted),
    ).toBe(true);
    expect(data.sources.find((s) => s.id === 'task-query-review')).toMatchObject({
      kind: 'independent',
      date: '2026-10-06',
      path: 'artifacts/task-query-review-2026-10-06/REVIEW.md',
      scope: expect.stringContaining('180 普通 / 最终覆盖 178 真实 PG / 8 doc / 15 独立边界'),
    });
    expect(data.deliveries.find((d) => d.id === 'foundation-task-query-v1')).toMatchObject({
      state: 'pending-review',
      reviewedAt: null,
    });
    expect(data.deliveries.find((d) => d.id === 'foundation-task-query-v1-review')).toMatchObject({
      id: 'foundation-task-query-v1-review',
      state: 'accepted',
      reviewedAt: '2026-10-06',
      taskIds: ['BASE-05'],
      sourceIds: ['task-query-review', 'task-query-contract', 'task-query-handoff'],
    });
  });
  it('preserves planning intake acceptance and its pending implementation history', () => {
    const data = progressSchema.parse(load());
    const task = data.tasks.find((t) => t.id === 'BASE-06')!;
    expect(task).toMatchObject({ status: 'partial', implemented: false, accepted: false });
    expect(task.subtasks.find((s) => s.id === 'planning-intake')).toMatchObject({
      implemented: true,
      accepted: true,
      sourceIds: ['planning-intake-contract', 'planning-intake-handoff', 'planning-intake-review'],
    });
    expect(task.subtasks.find((s) => s.id === 'real-llm')).toMatchObject({
      implemented: false,
      accepted: false,
    });
    expect(task.subtasks.find((s) => s.id === 'end-to-end')).toMatchObject({
      implemented: false,
      accepted: false,
    });
    expect(data.deliveries.find((d) => d.id === 'foundation-planning-intake-v1')).toMatchObject({
      state: 'pending-review',
      reviewedAt: null,
      taskIds: ['BASE-06'],
      result: expect.stringContaining('独立验收待检查'),
      sourceIds: ['planning-intake-contract', 'planning-intake-handoff'],
    });
    expect(data.sources.find((s) => s.id === 'planning-intake-review')).toMatchObject({
      kind: 'independent',
      date: '2026-10-06',
      path: 'artifacts/planning-intake-review-2026-10-06/REVIEW.md',
      scope: expect.stringContaining('199 普通 / 178 真实 PG / 9 doc / 18 独立边界'),
    });
    expect(
      data.deliveries.find((d) => d.id === 'foundation-planning-intake-v1-review'),
    ).toMatchObject({
      id: 'foundation-planning-intake-v1-review',
      state: 'accepted',
      reviewedAt: '2026-10-06',
      taskIds: ['BASE-06'],
      sourceIds: ['planning-intake-review', 'planning-intake-contract', 'planning-intake-handoff'],
    });
    expect(data.sources.find((s) => s.id === 'planning-intake-handoff')).toMatchObject({
      kind: 'implementation',
      scope: expect.stringContaining('尚未独立验收'),
    });
  });
  it('accepts only persistence session guards and preserves the original pending delivery', () => {
    const data = progressSchema.parse(load());
    const task = data.tasks.find((t) => t.id === 'BASE-05')!;
    expect(task).toMatchObject({ status: 'partial', implemented: false, accepted: false });
    expect(task.subtasks.find((s) => s.id === 'persistence-session')).toMatchObject({
      implemented: true,
      accepted: true,
      sourceIds: ['persistence-session-handoff', 'persistence-session-review'],
    });
    expect(task.subtasks.find((s) => s.id === 'task-query')?.accepted).toBe(true);
    expect(task.subtasks.find((s) => s.id === 'wait-recovery')?.accepted).toBe(true);
    expect(data.sources.find((s) => s.id === 'persistence-session-handoff')).toMatchObject({
      kind: 'implementation',
      path: 'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_持久化入口会话复核_v0.1.md',
    });
    expect(data.sources.find((s) => s.id === 'persistence-session-review')).toMatchObject({
      kind: 'independent',
      date: '2026-10-06',
      path: 'artifacts/persistence-session-review-2026-10-06/REVIEW.md',
      scope: expect.stringContaining('199 普通 / 191 真实 PG / 9 doc / 8 独立边界'),
    });
    expect(data.deliveries.find((d) => d.id === 'foundation-persistence-session-v1')).toMatchObject(
      {
        id: 'foundation-persistence-session-v1',
        state: 'pending-review',
        reviewedAt: null,
        taskIds: ['BASE-05'],
      },
    );
    expect(
      data.deliveries.find((d) => d.id === 'foundation-persistence-session-v1-review'),
    ).toMatchObject({
      id: 'foundation-persistence-session-v1-review',
      state: 'accepted',
      reviewedAt: '2026-10-06',
      taskIds: ['BASE-05'],
      sourceIds: ['persistence-session-review', 'persistence-session-handoff'],
      declared: expect.stringContaining('foundation-persistence-session-v1'),
      result: expect.stringContaining('199 普通、191 真实 PG、9 doc、8 独立边界'),
    });
  });
  it('accepts only the local directory check child while preserving pending history and its display ordering', () => {
    const data = progressSchema.parse(load());
    const task = data.tasks.find((task) => task.id === 'BASE-01')!;
    expect(task).toMatchObject({ status: 'partial', implemented: false, accepted: false });
    expect(task.subtasks.find((child) => child.id === 'directory-ownership')).toMatchObject({
      implemented: true,
      accepted: true,
      sourceIds: [
        'directory-ownership-handoff',
        'directory-ownership-generation',
        'directory-ownership-review',
      ],
    });
    expect(task.subtasks.find((child) => child.id === 'member-reproduction')).toMatchObject({
      implemented: false,
      accepted: false,
    });
    expect(
      data.deliveries.find((delivery) => delivery.id === 'foundation-directory-ownership-v1'),
    ).toMatchObject({
      id: 'foundation-directory-ownership-v1',
      state: 'pending-review',
      reviewedAt: null,
      taskIds: ['BASE-01'],
    });
    expect(
      data.deliveries.find(
        (delivery) => delivery.id === 'foundation-directory-ownership-v1-review',
      ),
    ).toMatchObject({
      id: 'foundation-directory-ownership-v1-review',
      state: 'accepted',
      reviewedAt: '2026-10-06',
      taskIds: ['BASE-01'],
      sourceIds: [
        'directory-ownership-review',
        'directory-ownership-handoff',
        'directory-ownership-generation',
      ],
      declared: expect.stringContaining('foundation-directory-ownership-v1'),
    });
    const historyIds = [
      'foundation-directory-ownership-v1',
      'foundation-directory-ownership-v1-review',
    ];
    expect(
      data.deliveries
        .filter((delivery) => historyIds.includes(delivery.id))
        .map((delivery) => delivery.id),
    ).toEqual(historyIds);
    expect(
      orderedDeliveries(data)
        .filter((delivery) => historyIds.includes(delivery.id))
        .map((delivery) => delivery.id),
    ).toEqual([...historyIds].reverse());
    expect(data.sources.find((source) => source.id === 'directory-ownership-review')).toMatchObject(
      {
        kind: 'independent',
        date: '2026-10-06',
        path: 'artifacts/ownership-check-review-2026-10-06/REVIEW.md',
        scope: expect.stringContaining('29项永久 / 13组独立 / 64项前端'),
      },
    );
    expect(
      data.sources.find((source) => source.id === 'directory-ownership-handoff'),
    ).toMatchObject({
      kind: 'implementation',
      path: 'artifacts/foundation-draft-lifecycle-2026-10-04/docs/implementation/04-local-directory-ownership.md',
    });
  });
  it('keeps transfer reports separate from independently verified foundation patches', () => {
    const data = progressSchema.parse(load());
    const trf = data.tasks.find((t) => t.id === 'TRF-02')!;
    expect(trf.subtasks[0]).toMatchObject({
      implemented: true,
      accepted: false,
      sourceIds: ['transfer-report'],
    });
    const delivery = data.deliveries.find((d) => d.version === '7cea089')!;
    expect(delivery).toMatchObject({ state: 'member-report', reviewedAt: null, receivedAt: null });
  });
  it('distinguishes received materials from unreported progress and acceptance', () => {
    const data = progressSchema.parse(load());
    const unknownIds = ['AUD-04'];
    const unknown = data.tasks.filter((t) => unknownIds.includes(t.id));
    expect(unknown.map((t) => t.id).sort()).toEqual([...unknownIds].sort());
    expect(unknown.every((t) => t.status === 'unknown' && !t.implemented && !t.accepted)).toBe(
      true,
    );
    const feedback = data.tasks.find((t) => t.id === 'BILL-04')!;
    const feedbackReviewId = 'billing-public-feedback-composition-r4-review-20261021';
    expect(feedback).toMatchObject({
      status: 'partial',
      implemented: false,
      accepted: false,
      sourceIds: expect.arrayContaining([feedbackReviewId]),
      scope: expect.stringContaining('保存反馈不授权锁卡'),
      completed: expect.arrayContaining([
        expect.stringContaining('公共组合报告已覆盖可信原报告归档/恢复'),
        expect.stringContaining('本轮不重跑或累计'),
      ]),
      gaps: expect.arrayContaining([
        expect.stringContaining('真实报告/消费来源、正式账户和卡归属仍待采用'),
        expect.stringContaining(
          '反馈到卡最终同连接复读、公共反馈ID/revision、D20更正撤销及正式审计映射仍未采用',
        ),
      ]),
    });
    const feedbackReview = data.sources.find((s) => s.id === feedbackReviewId)!;
    expect(feedbackReview).toMatchObject({
      kind: 'independent',
      date: '2026-10-21',
      path: 'artifacts/billing-public-feedback-composition-2026-10-21/REVIEW-INITIAL.md',
    });
    for (const scope of [
      '本地显式公共反馈接入限定通过',
      '原报告可信归档/恢复',
      '成员SAVEPOINT/公共结果/Outbox/任务终结',
      '原号GET',
      '非正式开放、真实供数、卡锁或整包通过',
    ])
      expect(feedbackReview.scope).toContain(scope);
    const received = data.tasks.filter((t) =>
      ['BILL-01', 'BILL-02', 'AUD-01', 'AUD-02', 'AUD-03', 'SUB-01', 'SUB-02', 'SUB-03'].includes(
        t.id,
      ),
    );
    expect(received.map((t) => t.id).sort()).toEqual(
      ['BILL-01', 'BILL-02', 'AUD-01', 'AUD-02', 'AUD-03', 'SUB-01', 'SUB-02', 'SUB-03'].sort(),
    );
    expect(received.find((t) => t.id === 'BILL-02')).toMatchObject({
      status: 'partial',
      implemented: false,
      accepted: false,
    });
    const billingReport = data.tasks.find((t) => t.id === 'BILL-03')!;
    expect(billingReport).toMatchObject({
      status: 'partial',
      implemented: false,
      accepted: false,
    });
    expect(billingReport.subtasks.find((s) => s.id === 'billing-local-basic-report')).toMatchObject(
      {
        implemented: true,
        accepted: true,
      },
    );
    expect(billingReport.subtasks.filter((s) => s.accepted).map((s) => s.id)).toEqual([
      'billing-local-basic-report',
    ]);
    for (const [id, status] of [
      ['AUD-03', 'partial'],
      ['SUB-02', 'pending-integration'],
      ['SUB-03', 'pending-integration'],
    ]) {
      expect(received.find((t) => t.id === id)).toMatchObject({
        status,
        implemented: false,
        accepted: false,
      });
    }
    expect(received.every((t) => t.status !== 'unknown' && !t.implemented && !t.accepted)).toBe(
      true,
    );
    for (const id of [
      'transfer-feishu-report-20261013',
      'billing-feishu-report-20261013',
      'subscription-feishu-report-20261013',
      'audit-draft-demo-report-20261015',
    ]) {
      const report = data.deliveries.find((delivery) => delivery.id === id)!;
      expect(report).toMatchObject({ state: 'member-report', reviewedAt: null });
      for (const taskId of report.taskIds) {
        expect(data.tasks.find((task) => task.id === taskId)?.accepted).toBe(false);
      }
    }
    const historicalReceiptIds = [
      'github-card01-20261007',
      'github-card02-20261007',
      'github-sub01-20261007',
      'github-audit01-20261007',
    ];
    for (const id of historicalReceiptIds) {
      expect(data.deliveries.find((d) => d.id === id)).toMatchObject({
        state: 'received',
        reviewedAt: null,
        sourceIds: expect.arrayContaining(['github-progress-20261007']),
      });
    }
    expect(
      data.deliveries.find((d) => d.id === 'billing-three-state-report-received'),
    ).toMatchObject({
      state: 'member-report',
      reviewedAt: null,
      sourceIds: ['billing-report-20261007', 'team-integration-received'],
    });
  });
  it('rejects an acceptance based only on member report or planning material', () => {
    const raw = load();
    raw.tasks.find((t: { id: string }) => t.id === 'TRF-02').subtasks[0].accepted = true;
    expect(progressSchema.safeParse(raw).success).toBe(false);
    const plan = load();
    plan.tasks[0].subtasks[0].sourceIds = ['plan-foundation'];
    expect(progressSchema.safeParse(plan).success).toBe(false);
  });
  it('rejects skipped necessary child, remaining gaps, contradictory unknown and bad references', () => {
    for (const mutate of [
      (r: ReturnType<typeof load>) => {
        r.tasks[0].implemented = true;
        r.tasks[0].accepted = true;
        r.tasks[0].status = 'accepted';
      },
      (r: ReturnType<typeof load>) => {
        r.tasks[0].status = 'unknown';
      },
      (r: ReturnType<typeof load>) => {
        r.tasks[0].sourceIds = ['missing-source'];
      },
      (r: ReturnType<typeof load>) => {
        r.sources[0].date = '2026-02-30';
      },
      (r: ReturnType<typeof load>) => {
        r.tasks[1].id = r.tasks[0].id;
      },
      (r: ReturnType<typeof load>) => {
        r.sources[0].url = 'javascript:alert(1)';
      },
    ]) {
      const raw = load();
      mutate(raw);
      expect(progressSchema.safeParse(raw).success).toBe(false);
    }
  });
  it('filters by owner, exact status and long versions while keeping empty results', () => {
    const data = progressSchema.parse(load());
    // Fix the version only in this isolated snapshot, independent of later ledger deliveries.
    data.tasks.find((t) => t.id === 'CARD-01')!.version =
      '2e464c41b2b3d60256fbda765f3a5d97473a6a33';
    expect(filterTasks(data, '2e464c41b2b3', 'cards', 'partial').map((t) => t.id)).toEqual([
      'CARD-01',
    ]);
    expect(filterTasks(data, 'TRF-04', 'transfer', 'pending-integration').map((t) => t.id)).toEqual(
      ['TRF-04'],
    );
    expect(filterTasks(data, '不存在', 'all', 'all')).toEqual([]);
    expect(memberCounts(data.tasks.filter((t) => t.ownerId === 'transfer')).accepted).toBe(0);
    const transferDeliveries = orderedDeliveries(data).filter((d) => d.ownerId === 'transfer');
    for (const delivery of transferDeliveries) {
      if (delivery.state === 'member-report') {
        expect(delivery.reviewedAt).toBeNull();
      }
      if (delivery.state === 'accepted') {
        expect(delivery.reviewedAt).not.toBeNull();
        expect(
          delivery.sourceIds.some((id) =>
            data.sources.some(
              (source) => source.id === id && source.kind === 'independent' && source.date !== null,
            ),
          ),
        ).toBe(true);
      }
    }
    expect(
      transferDeliveries.find((d) => d.id === 'transfer-business-store-review-20261016'),
    ).toMatchObject({
      state: 'accepted',
      reviewedAt: '2026-10-16',
      location: 'artifacts/transfer-c0e21f6-review-2026-10-16/REVIEW.md',
      scope: expect.stringContaining('仅内存资金逻辑'),
      result: expect.stringContaining('完整资金安全链仍未接'),
    });
  });
  it('failed sync preserves the last validated snapshot and successful-read time', () => {
    const first = syncSucceeded(load(), '2026-10-06T03:00:00Z');
    expect(syncFailed(first)).toEqual({ ...first, status: 'unsynced' });
    expect(syncFailed(initialSyncState)).toMatchObject({
      data: null,
      lastSuccessAt: null,
      status: 'unsynced',
    });
    const invalid = load();
    invalid.schemaVersion = 2;
    expect(() => syncSucceeded(invalid)).toThrow();
  });
  it('accepted filtering includes child evidence but leaves partial parents unchecked', () => {
    const data = progressSchema.parse(load());
    const tasks = filterTasks(data, '', 'foundation', 'accepted');
    expect(tasks.map((t) => t.id)).toContain('BASE-03');
    expect(tasks.every((t) => t.status === 'partial' && !t.accepted)).toBe(true);
    const cardTasks = filterTasks(data, '', 'cards', 'accepted');
    expect(cardTasks.map(({ id, status, accepted }) => ({ id, status, accepted }))).toEqual([
      { id: 'CARD-01', status: 'partial', accepted: false },
      { id: 'CARD-02', status: 'partial', accepted: false },
    ]);
    const cardTools = cardTasks.find((t) => t.id === 'CARD-02')!;
    expect(cardTools.subtasks.find((s) => s.id === 'public-card-read-navigation')).toMatchObject({
      implemented: true,
      accepted: true,
      sourceIds: expect.arrayContaining([
        'card-pr23-readonly-public-review-20261021',
        'card-pr23-readonly-install-review-20261021',
      ]),
    });
    expect(cardTools.subtasks.find((s) => s.id === 'formal-card-freeze-entry')).toMatchObject({
      implemented: false,
      accepted: false,
    });
    expect(memberCounts(data.tasks.filter((t) => t.ownerId === 'transfer')).implemented).toBe(3);
  });
  it('searches dependency material and preserves shared owners independent of task ownership', () => {
    // Filtering behavior must not depend on the day's live SUB-01 milestone.
    const original = progressSchema.parse(load());
    const data = structuredClone(original);
    expect(filterDependencies(data, '吊销', 'all', 'all').map((d) => d.id)).toEqual([
      'formal-auth-source',
    ]);
    expect(filterDependencies(data, '无此条目xyz', 'all', 'all')).toEqual([]);
    expect(filterDependencies(data, '', 'audit', 'all').map((d) => d.id)).toEqual([
      'audit-material',
      'p1-subscription',
    ]);
    expect(filterDependencies(data, '订阅', 'audit', 'unknown').map((d) => d.id)).toEqual([]);
    const ids = ['SUB-01', 'SUB-02', 'SUB-03'];
    const flags = (snapshot: Progress) =>
      snapshot.tasks
        .filter((task) => ids.includes(task.id))
        .map((task) => ({
          id: task.id,
          implemented: task.implemented,
          accepted: task.accepted,
          children: task.subtasks.map(({ id, implemented, accepted }) => ({
            id,
            implemented,
            accepted,
          })),
        }));
    for (const scenario of [
      {
        statuses: ['pending-review', 'pending-integration', 'pending-integration'],
        review: true,
        integration: true,
      },
      {
        statuses: ['pending-integration', 'pending-integration', 'pending-integration'],
        review: false,
        integration: true,
      },
      {
        statuses: ['pending-review', 'pending-review', 'pending-review'],
        review: true,
        integration: false,
      },
    ] as const) {
      const fixture = structuredClone(original);
      ids.forEach((id, index) => {
        fixture.tasks.find((task) => task.id === id)!.status = scenario.statuses[index];
      });
      expect(
        filterDependencies(fixture, '订阅', 'audit', 'pending-review')
          .map((d) => d.id)
          .includes('p1-subscription'),
      ).toBe(scenario.review);
      expect(
        filterDependencies(fixture, '订阅', 'audit', 'pending-integration')
          .map((d) => d.id)
          .includes('p1-subscription'),
      ).toBe(scenario.integration);
      for (const owner of ['cards', 'bills', 'audit']) {
        expect(filterDependencies(fixture, '订阅', owner, 'all').map((d) => d.id)).toContain(
          'p1-subscription',
        );
      }
      expect(filterDependencies(fixture, '订阅', 'transfer', 'all').map((d) => d.id)).not.toContain(
        'p1-subscription',
      );
      expect(flags(fixture)).toEqual(flags(original));
      for (const task of fixture.tasks.filter((task) => ids.includes(task.id))) {
        expect(task).toMatchObject({ implemented: false, accepted: false });
      }
    }
  });
  it('received, reported, pending, failed and accepted deliveries have separate labels and dates', () => {
    const data = progressSchema.parse(load());
    expect(new Set(Object.values(deliveryLabels)).size).toBe(5);
    const raw = load();
    const sourceCount = raw.sources.length;
    const review = raw.sources.find((source: { kind: string }) => source.kind === 'review');
    expect(review).toBeDefined();
    raw.deliveries.push({
      ...raw.deliveries[0],
      id: 'failed-delivery',
      state: 'changes-requested',
      reviewedAt: '2026-10-06',
      sourceIds: [review.id],
    });
    expect(raw.sources).toHaveLength(sourceCount);
    const failed = progressSchema.parse(raw).deliveries.at(-1)!;
    expect(deliveryDate(failed)).toEqual({ value: '2026-10-06', label: '复核日期' });
    expect(
      deliveryDate({
        ...data.deliveries[0],
        state: 'received',
        receivedAt: null,
        reviewedAt: null,
      }),
    ).toEqual({ value: null, label: '收到日期' });
    raw.deliveries.at(-1).state = 'accepted';
    expect(progressSchema.safeParse(raw).success).toBe(false);
  });
});
