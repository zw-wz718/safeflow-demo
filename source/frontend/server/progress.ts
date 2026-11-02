import { readFile, realpath, stat } from 'node:fs/promises';
import { isAbsolute, relative, resolve, extname, sep } from 'node:path';
import type { IncomingMessage, ServerResponse } from 'node:http';
import type { Plugin } from 'vite';
import { progressSchema } from '../src/progress/schema';

// Only these reviewed text sources can be read. Ledger edits cannot expand file access.
export const evidenceRegistry: Record<string, string> = {
  'startup-cargo-resolution-review-20261024':
    'artifacts/startup-cargo-resolution-2026-10-24/REVIEW.md',
  'card-freeze-domain-audit-design-review-20261024':
    'artifacts/card-freeze-domain-audit-design-2026-10-24/REVIEW.md',
  'card-freeze-domain-audit-design-initial-20261024':
    'artifacts/card-freeze-domain-audit-design-2026-10-24/REVIEW-INITIAL.md',
  'billing-clippy-completion-review-20261024':
    'artifacts/billing-clippy-completion-2026-10-24/REVIEW.md',
  'payment-process-db-recovery-review-20261024':
    'artifacts/payment-process-db-recovery-2026-10-24/REVIEW.md',
  'default-startup-final-review-20261024': 'artifacts/default-startup-repair-2026-10-24/REVIEW.md',
  'common-source-package-initial-review-20261024':
    'artifacts/common-source-package-2026-10-23/REVIEW-INITIAL.md',
  'common-source-package-repair1-review-20261024':
    'artifacts/common-source-package-2026-10-23/repair1/REVIEW.md',
  'bills-feedback-consumer-initial-delivery-20261023':
    'artifacts/bills-feedback-http-consumer-2026-10-23/DELIVERY.md',
  'bills-feedback-consumer-initial-review-20261023':
    'artifacts/bills-feedback-http-consumer-2026-10-23/REVIEW-INITIAL-VALIDATION.md',
  'bills-feedback-consumer-repair1-delivery-20261023':
    'artifacts/bills-feedback-http-consumer-2026-10-23/repair1/DELIVERY.md',
  'bills-feedback-consumer-final-review-20261023':
    'artifacts/bills-feedback-http-consumer-2026-10-23/REVIEW.md',
  'card-feedback-public-adoption-review-20261023':
    'artifacts/card-feedback-public-adoption-2026-10-23/REVIEW.md',
  'card-feedback-public-adoption-initial-review-20261023':
    'artifacts/card-feedback-public-adoption-2026-10-23/REVIEW-INITIAL.md',
  'card-feedback-member-review-20261023':
    'artifacts/card-feedback-intake-review-2026-10-23/CARD-REVIEW.md',
  'transfer-c368-compatibility-review-20261023':
    'artifacts/card-feedback-intake-review-2026-10-23/TRANSFER-REVIEW.md',
  'subscription-capture-receipts-review-20261023':
    'artifacts/subscription-capture-receipts-review-2026-10-23/REVIEW.md',
  'subscription-capture-public-adoption-review-20261023':
    'artifacts/subscription-capture-public-adoption-2026-10-23/REVIEW.md',
  'card-permit-workflow-review-20261023':
    'artifacts/subscription-capture-receipts-review-2026-10-23/CARD-REVIEW.md',
  'audit-actual-dual-review-20261023': 'artifacts/material-backlog-adoption-2026-10-23/REVIEW.md',
  'local-llm-settings-recovery-review-20261023':
    'artifacts/local-llm-settings-recovery-2026-10-23/REVIEW.md',
  'feishu-recent-intake-20261023': 'docs/handoff/FEISHU_RECENT_INTAKE_2026-10-23.md',
  'audit-96c7e5b-review-20261023': 'artifacts/member-deliveries-review-2026-10-23/AUDIT-REVIEW.md',
  'billing-f84e7e5-review-20261023':
    'artifacts/member-deliveries-review-2026-10-23/BILLING-REVIEW.md',
  'billing-typed-public-adoption-review-20261023':
    'artifacts/billing-typed-public-adoption-2026-10-23/REVIEW.md',
  'feishu-recent-intake-20261022': 'docs/handoff/FEISHU_RECENT_INTAKE_2026-10-22.md',
  'audit-ec6f797-review-20261022': 'artifacts/audit-ec6f797-review-2026-10-22/REVIEW.md',
  'subscription-c163-public-r2-review-20261022':
    'artifacts/subscription-c163-public-adoption-2026-10-22/REVIEW-R2.md',
  'subscription-c163-public-candidate-review-20261022':
    'artifacts/subscription-c163-public-adoption-2026-10-22/REVIEW-CANDIDATE.md',
  'subscription-c163-increment-review-20261022':
    'artifacts/subscription-c163-increment-review-2026-10-22/REVIEW.md',
  'subscription-c163-public-adoption-review-20261022':
    'artifacts/subscription-c163-public-adoption-2026-10-22/REVIEW.md',
  'next-integrations-intake-20261022': 'artifacts/next-intake-2026-10-22/INTAKE.md',
  'transfer-card-new-bridges-review-20261022':
    'artifacts/transfer-card-bridges-intake-2026-10-22/REVIEW.md',
  'transfer-card-common-composition-review-20261022':
    'artifacts/transfer-card-common-composition-2026-10-22/REVIEW.md',
  'card-pr23-2b7880-review-20261021': 'artifacts/card-pr23-intake-2026-10-21/REVIEW.md',
  'sqlx-unified-security-review-20261021':
    'artifacts/sqlx-unified-security-integration-2026-10-21/REVIEW-INITIAL.md',
  'card-pr23-readonly-public-review-20261021':
    'artifacts/card-pr23-readonly-public-adoption-2026-10-21/REVIEW-INITIAL.md',
  'card-pr23-readonly-install-review-20261021':
    'artifacts/card-pr23-readonly-public-adoption-2026-10-21/REVIEW-INSTALL.md',
  'audit-worker-install-review-20261021':
    'artifacts/audit-worker-handoff-2026-10-21/REVIEW-INSTALL.md',
  'transfer-card-guard-33cc-review-20261021':
    'artifacts/transfer-card-guard-intake-2026-10-21/REVIEW.md',
  'transfer-checked-card-adoption-review-20261021':
    'artifacts/transfer-checked-card-adoption-2026-10-21/REVIEW-INSTALL.md',
  'subscription-public-withdrawal-r1-review-20261021':
    'artifacts/subscription-public-composition-2026-10-21/REVIEW-WITHDRAWAL-R1.md',
  'subscription-public-v1-initial-review-20261021':
    'artifacts/subscription-public-composition-2026-10-21/REVIEW-INITIAL.md',
  'billing-public-feedback-composition-r4-review-20261021':
    'artifacts/billing-public-feedback-composition-2026-10-21/REVIEW-INITIAL.md',
  'billing-feedback-8ff8c665-review-20261021':
    'artifacts/billing-feedback-intake-2026-10-21/REVIEW.md',
  'audit-sender-repair1-review-20261021':
    'artifacts/audit-v04-foundation-sender-2026-10-21/REVIEW-REPAIR1.md',
  'audit-sender-initial-review-20261021':
    'artifacts/audit-v04-foundation-sender-2026-10-21/REVIEW-INITIAL.md',
  'audit-v04-review-20261021':
    'artifacts/audit-v04-independent-review-2026-10-21/REVIEW-AUD-03-V04.md',
  'payment-http-web-rereview3-20261020':
    'artifacts/payment-http-web-connection-2026-10-20/REVIEW-REREVIEW-3.md',
  'payment-http-web-rereview1-20261020':
    'artifacts/payment-http-web-connection-2026-10-20/REVIEW-REREVIEW-1.md',
  'payment-http-web-rereview2-20261020':
    'artifacts/payment-http-web-connection-2026-10-20/REVIEW-REREVIEW-2.md',
  'payment-http-web-initial-review-20261020':
    'artifacts/payment-http-web-connection-2026-10-20/REVIEW-INITIAL.md',
  'payment-http-web-generation-20261020':
    'artifacts/payment-http-web-connection-2026-10-20/DELIVERY.md',
  'payment-http-web-review-20261020': 'artifacts/payment-http-web-connection-2026-10-20/REVIEW.md',
  'feishu-reply-intake-20261021': 'docs/handoff/FEISHU_REPLY_INTAKE_2026-10-21.md',
  'audit-hash-v04-review-note-20261020':
    'artifacts/feishu-replies-intake-2026-10-20/AUD-03_v0.4_交付与复审说明.md',
  'audit-hash-v04-foundation-task-20261020':
    'artifacts/feishu-replies-intake-2026-10-20/AUD-03_v0.4_交付给Echo_底座接入任务.md',
  'audit-hash-v04-capability-20261020':
    'artifacts/feishu-replies-intake-2026-10-20/AUD-03_Hash_Chain与核验能力说明_v0.4.md',
  'card-public-composition-initial-review-20261020':
    'artifacts/card-public-payment-composition-2026-10-19/REVIEW-INITIAL.md',
  'card-public-composition-generation-20261019':
    'artifacts/card-public-payment-composition-2026-10-19/DELIVERY.md',
  'card-public-composition-review-20261019':
    'artifacts/card-public-payment-composition-2026-10-19/REVIEW.md',
  'member-delivery-review-20261019':
    'artifacts/member-delivery-review-2026-10-19-evening/REVIEW.md',
  'card-delivery-review-20261019':
    'artifacts/member-delivery-review-2026-10-19-evening/CARD-REVIEW.md',
  'subscription-delivery-review-20261019':
    'artifacts/member-delivery-review-2026-10-19-evening/SUBSCRIPTION-REVIEW.md',
  'audit-delivery-review-20261019':
    'artifacts/member-delivery-review-2026-10-19-evening/AUDIT-REVIEW.md',
  'agent-main-flow-review': 'artifacts/agent-main-flow-review-2026-10-11/REVIEW.md',
  'foundation-delivery-review': 'artifacts/foundation-delivery-review-2026-10-11/REVIEW.md',
  'agent-fixed-flow-generation': 'artifacts/agent-fixed-flow-2026-10-11/REPORT.md',
  'agent-main-flow-generation': 'artifacts/agent-main-flow-2026-10-10/REPORT.md',
  'persisted-lifecycle-v2-review': 'artifacts/persisted-lifecycle-v2-review-2026-10-09/REVIEW.md',
  'persisted-lifecycle-v2-generation':
    'artifacts/persisted-lifecycle-v2-resume-2026-10-09/REPORT.md',
  'persisted-lifecycle-v2-contract':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/contracts/26-persisted-mock-draft-lifecycle-http-v2.md',
  'persisted-lifecycle-design-review':
    'artifacts/persisted-lifecycle-design-review-2026-10-09/REVIEW.md',
  'persisted-lifecycle-design':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/design/persisted-mock-draft-lifecycle-v2-proposal.md',
  'current-draft-review': 'artifacts/current-draft-review-2026-10-09/REVIEW.md',
  'current-draft-generation': 'artifacts/persisted-lifecycle-generation-2026-10-09/REPORT.md',
  'current-draft-contract':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/contracts/25-current-mock-draft-observation-http-v1.md',
  'persisted-draft-received': 'artifacts/persisted-draft-review-2026-10-08/RECEIVED.md',
  'persisted-draft-findings': 'artifacts/persisted-draft-review-2026-10-08/FINDINGS.md',
  'persisted-draft-review': 'artifacts/persisted-draft-review-2026-10-08/REVIEW.md',
  'persisted-draft-contract':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/contracts/24-persisted-mock-draft-execution-http.md',
  'billing-doc-revision-52': 'docs/progress/collections/jonathan-doc-revision-52.json',
  'team-integration-handoff': 'docs/handoff/SafeFlow_三方接入核对与回复_v0.1.md',
  'team-integration-review': 'artifacts/team-integration-review-2026-10-08/REVIEW.md',
  'team-integration-received': 'artifacts/team-integration-review-2026-10-08/RECEIVED.md',
  'billing-report-20261007': 'docs/handoff/SafeFlow_协作材料汇总.md',
  'llm-gateway-received': 'artifacts/llm-gateway-review-2026-10-08/RECEIVED.md',
  'llm-gateway-review': 'artifacts/llm-gateway-review-2026-10-08/REVIEW.md',
  'llm-gateway-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_OpenAI兼容模型规划_v0.1.md',
  'confirmation-http-received': 'artifacts/confirmation-http-review-2026-10-08/RECEIVED.md',
  'confirmation-http-review': 'artifacts/confirmation-http-review-2026-10-08/REVIEW.md',
  'confirmation-http-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_本机模拟草案公共确认_v0.1.md',
  'http-runtime-received': 'artifacts/http-runtime-review-2026-10-08/RECEIVED.md',
  'http-runtime-review': 'artifacts/http-runtime-review-2026-10-08/REVIEW.md',
  'http-runtime-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_本机只读HTTP与模拟会话_v0.1.md',
  'plan-foundation': 'SafeFlow_完整项目直拷版_2026-10-02/docs/collaboration/tasks/02_不高兴.md',
  'plan-transfer': 'SafeFlow_完整项目直拷版_2026-10-02/docs/collaboration/tasks/01_逐风.md',
  'plan-bills': 'SafeFlow_完整项目直拷版_2026-10-02/docs/collaboration/tasks/03_Jonathan.md',
  'plan-cards': 'SafeFlow_完整项目直拷版_2026-10-02/docs/collaboration/tasks/04_结夏.md',
  'plan-audit': 'SafeFlow_完整项目直拷版_2026-10-02/docs/collaboration/tasks/05_风星龙.md',
  'runtime-implementation':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/implementation/02-deterministic-runtime.md',
  'startup-implementation':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/implementation/03-startup-bindings.md',
  'lifecycle-review': 'artifacts/SafeFlow_草案修改取消安全入口_独立审查.md',
  'storage-review': 'artifacts/SafeFlow_底座第四步A_独立审查.md',
  'recovery-review': 'artifacts/SafeFlow_底座第四步B_独立审查.md',
  'confirmation-review': 'artifacts/confirmation-review-2026-10-05/REVIEW.md',
  'permit-review': 'artifacts/permit-review-2026-10-05/REVIEW.md',
  'auth-review': 'artifacts/auth-review-2026-10-05/REVIEW.md',
  'decision-review': 'artifacts/decision-review-2026-10-05/REVIEW.md',
  'decision-contract':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/contracts/14-trusted-preflight-decision-storage-v1.md',
  'task-query-review': 'artifacts/task-query-review-2026-10-06/REVIEW.md',
  'task-query-contract':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/contracts/15-task-progress-query-v1.md',
  'task-query-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_任务进度查询与安全范围视图_v0.1.md',
  'planning-intake-contract':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/contracts/16-model-planning-intake-v1.md',
  'planning-intake-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_模型计划安全接收_v0.1.md',
  'planning-intake-review': 'artifacts/planning-intake-review-2026-10-06/REVIEW.md',
  'persistence-session-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_持久化入口会话复核_v0.1.md',
  'persistence-session-review': 'artifacts/persistence-session-review-2026-10-06/REVIEW.md',
  'directory-ownership-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/implementation/04-local-directory-ownership.md',
  'directory-ownership-generation': 'artifacts/ownership-check-generation-2026-10-06/REPORT.md',
  'directory-ownership-review': 'artifacts/ownership-check-review-2026-10-06/REVIEW.md',
  'permit-gates-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_许可强验证门禁存储兼容_v0.1.md',
  'permit-gates-finding': 'artifacts/permit-gates-review-2026-10-07/FINDING.md',
  'permit-gates-review': 'artifacts/permit-gates-review-2026-10-07/REVIEW.md',
  'precommit-contract-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_PRE_COMMIT条件性门禁公共合同_v0.1.md',
  'precommit-contract-review': 'artifacts/precommit-contract-review-2026-10-07/REVIEW.md',
  'source-combination-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_裁决与确认公共来源组合核验_v0.1.md',
  'source-combination-finding': 'artifacts/evidence-composition-review-2026-10-07/FINDING.md',
  'source-combination-review': 'artifacts/evidence-composition-review-2026-10-07/REVIEW.md',
  'precommit-sources-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_PRE_COMMIT请求公共来源部分核对_v0.1.md',
  'precommit-sources-finding': 'artifacts/precommit-source-review-2026-10-07/FINDING.md',
  'precommit-sources-review': 'artifacts/precommit-source-review-2026-10-07/REVIEW.md',
  'planning-task-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_模型计划接收与持久任务创建_v0.1.md',
  'planning-task-review': 'artifacts/planning-persistence-review-2026-10-07/REVIEW.md',
  'frontend-review': 'artifacts/frontend-review/REVIEW.md',
  'frontend-verification': 'frontend/VERIFICATION.md',
  'transfer-report': 'docs/progress/sources/transfer-report.md',
  'card-review-excerpt': 'docs/progress/sources/card-review-excerpt.md',
  'github-progress-20261007': 'docs/progress/sources/github-progress-2026-10-07.md',
  'transfer-intake-20261008': 'docs/handoff/SafeFlow_协作材料汇总.md',
  'audit-wechat-20261008': 'docs/handoff/SafeFlow_协作材料汇总.md',
  'runtime-execution-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_已存任务只读Runtime调度_v0.1.md',
  'runtime-execution-review': 'artifacts/runtime-execution-review-2026-10-08/REVIEW.md',
  'runtime-continuation-handoff':
    'artifacts/foundation-draft-lifecycle-2026-10-04/docs/handoff/SafeFlow_原任务只读显式续跑_v0.1.md',
  'runtime-continuation-review': 'artifacts/runtime-continuation-review-2026-10-08/REVIEW.md',
};
const ledgerPath = 'docs/progress/project-progress.json';
function inside(root: string, path: string) {
  const rel = relative(root, path);
  return rel !== '' && !isAbsolute(rel) && rel !== '..' && !rel.startsWith(`..${sep}`);
}
export async function readProjectText(root: string, path: string, limit = 262144) {
  if (
    isAbsolute(path) ||
    path.includes('\\') ||
    path.split('/').some((p) => !p || p === '..' || p === '.') ||
    !['.md', '.txt', '.json'].includes(extname(path))
  )
    throw new Error('PATH_REJECTED');
  const rootReal = await realpath(root);
  const file = resolve(rootReal, path);
  if (!inside(rootReal, file)) throw new Error('PATH_REJECTED');
  const fileReal = await realpath(file);
  if (!inside(rootReal, fileReal)) throw new Error('PATH_REJECTED');
  const metadata = await stat(fileReal);
  if (!metadata.isFile() || metadata.size > limit) throw new Error('FILE_REJECTED');
  const content = await readFile(fileReal, 'utf8');
  if (Buffer.byteLength(content) > limit || content.includes('\0'))
    throw new Error('FILE_REJECTED');
  return content;
}
export function createProgressMiddleware(root: string, registry = evidenceRegistry) {
  return async (req: IncomingMessage, res: ServerResponse, next: () => void) => {
    const raw = req.url ?? '';
    if (!raw.startsWith('/api/progress')) return next();
    const send = (status: number, body: unknown) => {
      res.statusCode = status;
      res.setHeader('Content-Type', 'application/json; charset=utf-8');
      res.setHeader('Cache-Control', 'no-store');
      res.setHeader('X-Content-Type-Options', 'nosniff');
      res.end(JSON.stringify(body));
    };
    if (req.method !== 'GET')
      return send(405, { error: 'READ_ONLY', message: '进度接口只允许读取' });
    if (raw.includes('?') || raw.includes('#'))
      return send(400, { error: 'QUERY_REJECTED', message: '不接受路径或查询参数' });
    const match = /^\/api\/progress\/evidence\/([a-z][a-z0-9-]{1,80})$/.exec(raw);
    if (raw !== '/api/progress' && !match)
      return send(404, { error: 'NOT_REGISTERED', message: '证据未登记' });
    try {
      const data = progressSchema.parse(
        JSON.parse(await readProjectText(root, ledgerPath, 1048576)),
      );
      if (!match) return send(200, data);
      const source = data.sources.find((s) => s.id === match[1]);
      if (!source || !Object.hasOwn(registry, source.id) || registry[source.id] !== source.path)
        return send(404, { error: 'NOT_REGISTERED', message: '证据未登记或路径不匹配' });
      const content = await readProjectText(root, registry[source.id]);
      return send(200, { sourceId: source.id, content });
    } catch {
      // Never send filesystem errors, raw invalid data, or credentials to the browser.
      return send(503, {
        error: match ? 'EVIDENCE_UNAVAILABLE' : 'PROGRESS_UNAVAILABLE',
        message: match ? '证据暂时无法读取' : '台账暂时无法读取或未通过校验',
      });
    }
  };
}
export function progressPlugin(root: string): Plugin {
  return {
    name: 'safeflow-readonly-progress',
    configureServer(server) {
      server.middlewares.use(createProgressMiddleware(root));
    },
    configurePreviewServer(server) {
      server.middlewares.use(createProgressMiddleware(root));
    },
  };
}
