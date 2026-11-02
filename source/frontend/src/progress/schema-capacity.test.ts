import { describe, expect, it } from 'vitest';
import { officialTaskIds, ownerIds, progressSchema, type Progress } from './schema';

// Capacity checks need a valid reference graph whose size is independent of
// the live ledger. Keep each source kind used by the semantic rejection cases.
function capacityFixture() {
  const reviewedOn = '2026-10-21';
  const kinds = ['plan', 'independent', 'implementation', 'member-report', 'review'] as const;
  return progressSchema.parse({
    schemaVersion: 1,
    updatedAt: `${reviewedOn}T00:00:00Z`,
    current: {
      headline: 'Capacity test fixture',
      reviewedOn,
      sourceId: 'capacity-independent',
      limits: ['Test data only'],
      tests: { ordinary: 0, postgres: 0, doc: 0, independent: 0 },
    },
    members: ownerIds.map((id) => ({
      id,
      name: id,
      role: 'Capacity fixture',
      stage: 'Test data only',
      next: 'No project action',
    })),
    sources: kinds.map((kind) => ({
      id: `capacity-${kind}`,
      title: `Capacity ${kind}`,
      kind,
      path: 'capacity-fixture.md',
      date: reviewedOn,
      version: null,
      scope: 'Test data only',
    })),
    tasks: officialTaskIds.map((id) => ({
      id,
      ownerId: 'foundation',
      title: `Capacity ${id}`,
      priority: 'P1',
      requirements: ['Test data only'],
      planSourceId: 'capacity-plan',
      status: 'not-started',
      implemented: false,
      accepted: false,
      scope: 'Test data only',
      completed: [],
      gaps: [],
      next: 'No project action',
      version: null,
      sourceIds: ['capacity-plan'],
      subtasks: [],
    })),
    deliveries: [
      {
        id: 'capacity-delivery',
        ownerId: 'foundation',
        receivedAt: null,
        reviewedAt: null,
        recordedAt: reviewedOn,
        version: 'fixture',
        location: 'capacity-fixture',
        state: 'received',
        title: 'Capacity delivery',
        scope: 'Test data only',
        declared: 'Test data only',
        result: 'Test data only',
        taskIds: ['BASE-01'],
        sourceIds: ['capacity-implementation'],
      },
    ],
    dependencies: [],
  });
}

function withSources(count: number) {
  const data = capacityFixture();
  expect(data.sources.length).toBeLessThanOrEqual(count);
  const original = structuredClone(data.sources);
  const used = new Set(data.sources.map((source) => source.id));
  for (let serial = 0; data.sources.length < count; serial++) {
    const id = `capacity-fixture-${serial}`;
    if (used.has(id)) continue;
    used.add(id);
    data.sources.push({ ...original[0], id });
  }
  expect(data.sources.slice(0, original.length)).toEqual(original);
  expect(new Set(data.sources.map((source) => source.id)).size).toBe(count);
  return data;
}

function withDeliveries(count: number) {
  const data = capacityFixture();
  data.deliveries = Array.from({ length: count }, (_, index) => ({
    ...structuredClone(data.deliveries[0]),
    id: `capacity-delivery-${index}`,
  }));
  return data;
}

describe('bounded delivery history using an isolated fixture', () => {
  it.each([301, 500])('preserves %i distinct valid deliveries', (count) => {
    const data = withDeliveries(count);
    expect(progressSchema.parse(data)).toEqual(data);
  });

  it('rejects the 501st delivery specifically at the delivery limit', () => {
    const data = withDeliveries(501);
    expect(
      progressSchema.safeParse({
        ...data,
        deliveries: data.deliveries.slice(0, 500),
      }).success,
    ).toBe(true);
    const result = progressSchema.safeParse(data);
    expect(result.success).toBe(false);
    if (result.success) throw new Error('501 deliveries unexpectedly accepted');
    expect(result.error.issues).toEqual([
      expect.objectContaining({
        code: 'too_big',
        path: ['deliveries'],
        maximum: 500,
      }),
    ]);
  });

  it('checks duplicate IDs beyond the former 300-delivery boundary', () => {
    const data = withDeliveries(500);
    data.deliveries[499].id = data.deliveries[0].id;
    const result = progressSchema.safeParse(data);
    expect(result.success).toBe(false);
    if (result.success) throw new Error('Duplicate delivery unexpectedly accepted');
    expect(result.error.issues).toContainEqual(expect.objectContaining({ message: '交付ID重复' }));
  });

  it.each(['accepted', 'changes-requested'] as const)(
    'requires the correct source and review date for %s at delivery 500',
    (state) => {
      const data = withDeliveries(500);
      const delivery = data.deliveries[499];
      delivery.state = state;
      delivery.reviewedAt = '2026-10-21';
      // The opposite kind cannot stand in for the required review conclusion.
      delivery.sourceIds = [state === 'accepted' ? 'capacity-review' : 'capacity-independent'];
      expect(progressSchema.safeParse(data).success).toBe(false);
      delivery.sourceIds = [state === 'accepted' ? 'capacity-independent' : 'capacity-review'];
      expect(progressSchema.safeParse(data).success).toBe(true);
      delivery.reviewedAt = null;
      expect(progressSchema.safeParse(data).success).toBe(false);
    },
  );
});

describe('bounded progress source capacity using an isolated fixture', () => {
  it.each([101, 200])('parses %i unique sources without changing fixture sources', (count) => {
    const data = withSources(count);
    expect(progressSchema.parse(data)).toEqual(data);
  });

  it('rejects 201 otherwise valid unique sources specifically at the sources capacity', () => {
    const data = withSources(201);
    expect(progressSchema.safeParse({ ...data, sources: data.sources.slice(0, 200) }).success).toBe(
      true,
    );
    const result = progressSchema.safeParse(data);
    expect(result.success).toBe(false);
    if (result.success) throw new Error('201 sources unexpectedly accepted');
    expect(result.error.issues).toEqual([
      expect.objectContaining({
        code: 'too_big',
        path: ['sources'],
        maximum: 200,
        inclusive: true,
        type: 'array',
      }),
    ]);
  });

  it('still allows only 100 source references per delivery with 200 registered sources', () => {
    const data = withSources(200);
    const delivery = data.deliveries[0];
    delivery.state = 'received';
    delivery.reviewedAt = null;
    delivery.sourceIds = data.sources.slice(0, 100).map((source) => source.id);
    expect(progressSchema.safeParse(data).success).toBe(true);
    delivery.sourceIds.push(data.sources[100].id);
    const result = progressSchema.safeParse(data);
    expect(result.success).toBe(false);
    if (result.success) throw new Error('101 references unexpectedly accepted');
    expect(result.error.issues).toContainEqual(
      expect.objectContaining({
        code: 'too_big',
        path: ['deliveries', 0, 'sourceIds'],
        maximum: 100,
      }),
    );
  });

  const invalidCases: [string, (data: Progress) => void, string][] = [
    [
      'duplicate sources',
      (data) => {
        data.sources[199].id = data.sources[0].id;
      },
      '来源ID重复',
    ],
    [
      'unknown references',
      (data) => {
        data.deliveries[0].sourceIds = ['missing-capacity-source'];
      },
      '未知证据引用',
    ],
    [
      'member report as acceptance',
      (data) => {
        data.deliveries[0].state = 'accepted';
        data.deliveries[0].reviewedAt = '2026-10-21';
        data.deliveries[0].sourceIds = [
          data.sources.find((source) => source.kind === 'member-report')!.id,
        ];
      },
      '交付验收需要独立结果',
    ],
    [
      'failed review as acceptance',
      (data) => {
        data.deliveries[0].state = 'accepted';
        data.deliveries[0].reviewedAt = '2026-10-21';
        data.deliveries[0].sourceIds = [
          data.sources.find((source) => source.kind === 'review')!.id,
        ];
      },
      '交付验收需要独立结果',
    ],
    [
      'current review date mismatch',
      (data) => {
        data.current.reviewedOn = '2000-01-01';
      },
      '当前验收日期与来源不一致',
    ],
  ];
  it.each(invalidCases)(
    'retains semantic rejection for %s at 200 sources',
    (_label, mutate, message) => {
      const data = withSources(200);
      mutate(data);
      const result = progressSchema.safeParse(data);
      expect(result.success).toBe(false);
      if (result.success) throw new Error('Invalid semantics unexpectedly accepted');
      expect(result.error.issues).toContainEqual(expect.objectContaining({ message }));
    },
  );

  it('still rejects an unsafe source URL at 200 sources', () => {
    const data = withSources(200);
    data.sources[199].url = 'javascript:alert(1)';
    const result = progressSchema.safeParse(data);
    expect(result.success).toBe(false);
    if (result.success) throw new Error('Unsafe URL unexpectedly accepted');
    expect(result.error.issues).toContainEqual(
      expect.objectContaining({ path: ['sources', 199, 'url'] }),
    );
  });
});
