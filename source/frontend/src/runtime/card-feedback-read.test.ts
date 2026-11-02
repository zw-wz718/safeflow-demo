import { describe, expect, it } from 'vitest';
import { cardReadSchema } from './card-read';
const feedbackView = () => ({
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  scope: 'CARD_READ_ONLY',
  step_ref: '6dd861c2-54ba-450a-8c85-f021e9828090',
  funds_authorized: false,
  cards: [],
  account_outgoing_restricted: true,
  account_public_reasons: [],
  feedback: {
    state: 'CARD_ASSOCIATION_VERIFIED',
    message: '已核对应卡',
    scope_explanation: '同卡历史不等于本反馈已办理',
  },
  history: {
    last_four: '1234',
    card_locked: true,
    account_outgoing_restricted: true,
    message: '保护记录',
    ordering_explanation: '按本次位置明确选择',
    locks: [],
  },
  original: null,
  unlock_instruction: '请到线下银行办理',
});
const original = () => ({
  state: 'NEEDS_REVIEW',
  last_four: null,
  account_outgoing_restricted: null,
  public_reasons: [],
  message: '原结果需复核',
  original_action: null,
});
describe('feedback strict display contract', () => {
  it('accepts verified history without automatically selecting a record', () => {
    expect(cardReadSchema.parse(feedbackView()).original).toBeNull();
  });
  it.each(['STOP_MINE', 'STOP_UNSURE', 'STOP_NO_CARD'])(
    'accepts %s only with no selectable history',
    (state) => {
      const v = {
        ...feedbackView(),
        feedback: { ...feedbackView().feedback, state },
        history: null,
      };
      expect(cardReadSchema.safeParse(v).success).toBe(true);
      expect(cardReadSchema.safeParse({ ...v, history: feedbackView().history }).success).toBe(
        false,
      );
      expect(cardReadSchema.safeParse({ ...v, original: original() }).success).toBe(false);
    },
  );
  it('rejects switching cards and a verified association with missing history', () => {
    expect(cardReadSchema.safeParse({ ...feedbackView(), history: null }).success).toBe(false);
    expect(
      cardReadSchema.safeParse({
        ...feedbackView(),
        cards: [
          {
            position: 1,
            last_four: '1234',
            ambiguous_tail: false,
            card_locked: true,
            public_reasons: [],
          },
        ],
      }).success,
    ).toBe(false);
  });
  it('requires an explicit nullable current action and clears cached action for NeedsReview', () => {
    expect(cardReadSchema.safeParse({ ...feedbackView(), original: original() }).success).toBe(
      true,
    );
    for (const action of [undefined, '旧记录新建锁'])
      expect(
        cardReadSchema.safeParse({
          ...feedbackView(),
          original: { ...original(), original_action: action },
        }).success,
      ).toBe(false);
    expect(
      cardReadSchema.safeParse({
        ...feedbackView(),
        original: { ...original(), state: 'RECORDED_ACTIVE', original_action: null },
      }).success,
    ).toBe(false);
  });
  it.each(['source_id', 'owner', 'card_id', 'snapshot', 'feedback_operation_id'])(
    'rejects backend field %s',
    (key) => {
      expect(
        cardReadSchema.safeParse({
          ...feedbackView(),
          feedback: { ...feedbackView().feedback, [key]: 'private' },
        }).success,
      ).toBe(false);
    },
  );
});
