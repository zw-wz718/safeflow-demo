import { RuntimeApiError, RuntimeTransportError } from './client';
import { RecoveryError } from './billing-recovery-client';

export type ServiceScope =
  'agent' | 'home' | 'transfer' | 'bills' | 'cards' | 'subscriptions' | 'login';
export function runtimeErrorText(error: unknown, scope: ServiceScope) {
  if (error instanceof RuntimeApiError) {
    const ended: Record<string, string> = {
      DRAFT_INTENT_EXPIRED: '这份未付款草案已过期，旧确认不能继续使用。请查询原草案后重新办理；原记录保留。',
      DRAFT_INTENT_REPLACED: '这份未付款草案已被新意图替代，旧确认不能继续使用。请核对新草案并重新确认；原记录保留。',
      DRAFT_INTENT_ABANDONED: '这份未付款草案已明确放弃，旧确认不能继续使用。请查询原草案；原记录保留。',
    };
    if (ended[error.reason]) return ended[error.reason];
  }
  if (error instanceof RuntimeApiError && error.status === 401)
    return '登录已失效，请重新登录。办理记录已保留；重新登录后先查询进度，旧确认和验证码不会恢复。';
  if (error instanceof RecoveryError)
    return error.status === 401
      ? '登录已失效，请重新登录后查询原反馈。'
      : '这次反馈尚未完成核对，请按原反馈编号查询。未核实前不要重新提交或更改判断。';
  const action =
    scope === 'subscriptions'
      ? '请查询这次订阅办理结果，未核实前不要重复保存。'
      : scope === 'bills'
        ? '请重新读取账单或查询原反馈；保存反馈不代表交易真伪已判定。'
        : scope === 'transfer'
          ? '请按办理编号查询进度，结果未明时不要重新付款。'
          : scope === 'cards'
            ? '请查询这次卡片操作的实际状态。'
            : scope === 'login'
              ? '请核对模拟登录口令后重试。'
              : '请查询已有任务；余额、账单和卡片可独立查看。';
  if (error instanceof RuntimeTransportError) {
    const reason =
      error.reason === 'TIMEOUT'
        ? '等待响应超时，服务端结果尚未核实。'
        : error.reason === 'UNAVAILABLE'
          ? '连接暂不可用，未自动重发请求。'
          : error.reason === 'ABORTED'
            ? '本次读取已停止，历史记录已保留。'
            : '返回内容未通过校验，未据此确认业务成功。';
    return reason + action;
  }
  if (error instanceof RuntimeApiError) {
    const labels: Record<string, string> = {
      LLM_NOT_CONFIGURED: '智能理解服务尚未启用。',
      LLM_GATEWAY_UNAVAILABLE: '智能理解服务未返回完整可用响应，可能连接失败或超时；没有自动重发。',
      LLM_PROPOSAL_REJECTED: '理解结果未通过计划校验，没有据此执行业务。',
      DEPENDENCY_UNAVAILABLE: '本次所需资料或服务暂不可用。',
      FORBIDDEN: '账户或操作校验未通过；这不等于服务故障。',
      STALE_PLAN: '办理资料版本已变化，需要按原任务重新核对。',
      NOT_FOUND: '本次未查到对应记录，请保留已有编号核对。',
      PAYMENT_AUTHENTICATION_REQUIRED: '本次请求未获有效付款验证，验证可能未完成或已过期。',
      PAYMENT_CODE_INACTIVE: '本次付款验证码尚未领取、已过期、已使用或不可用。',
      PAYMENT_AUTHENTICATION_EXPIRED: '本次请求的付款验证已过期。',
      PAYMENT_CONFIRMATION_INVALID: '本次请求未获有效付款确认。',
      PAYMENT_PERMIT_INACTIVE: '本次请求未获有效付款许可。',
    };
    return (labels[error.reason] ?? '本次办理未完成，请核对实际状态。') + action;
  }
  // Application messages are controlled locally. Do not render schema dumps,
  // provider responses or arbitrary object contents as user-facing diagnostics.
  if (
    error instanceof Error &&
    !/^[\s]*[\[{]/.test(error.message) &&
    !/[A-Z_]{6,}/.test(error.message)
  )
    return error.message + '。' + action;
  return '资料未通过核对，未据此确认业务成功。' + action;
}
