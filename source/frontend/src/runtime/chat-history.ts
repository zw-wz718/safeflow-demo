// Display history only. Financial selectors, IndexedDB and bank records are not cleared.
const prefix = 'safeflow.bank.chat.v2.';
const oldPrefix = 'safeflow.bank.chat.v1.';
const markerPrefix = 'safeflow.bank.chat-display-generation.v2.';
const originalsPrefix = 'safeflow.bank.chat-originals.v1.';
const identifier = /^[A-Za-z0-9_.-]{1,200}$/;
type Originals = { tasks: string[]; blockers: string[] };

function ids(value: unknown): string[] {
  if (!Array.isArray(value) || value.length > 256 || value.some(v => typeof v !== 'string' || !identifier.test(v)))
    throw Error('旧聊天的恢复编号无法核验，已停止新请求；请保留原号查询。');
  return [...new Set(value as string[])];
}
function merge(...values: string[][]) { return ids([...new Set(values.flat())]); }
function originalState(context: string): Originals {
  const raw = sessionStorage.getItem(originalsPrefix + context);
  if (!raw) return { tasks: [], blockers: [] };
  const v = JSON.parse(raw);
  return { tasks: ids(v.tasks), blockers: ids(v.blockers) };
}
function writeOriginals(context: string, value: Originals) {
  const key = originalsPrefix + context, raw = JSON.stringify(value);
  sessionStorage.setItem(key, raw);
  if (sessionStorage.getItem(key) !== raw) throw Error('无法保留办理原号，已停止清理。');
}
function preserve(prior: Originals, raw: string | null): Originals {
  if (!raw) throw Error('旧聊天的恢复线索缺失，已停止清理。');
  const value = JSON.parse(raw);
  if (!value || typeof value !== 'object' || Array.isArray(value))
    throw Error('旧聊天的恢复线索无法核验，已停止清理。');
  const tasks = ids(value.tasks);
  // Empty selection is valid; corrupt/missing IDs and non-boolean pending are not.
  if (typeof value.active !== 'string' || (value.active !== '' && !identifier.test(value.active)) ||
      typeof value.pending !== 'boolean' || (value.pending && value.active === ''))
    throw Error('旧聊天的恢复编号无法核验，已停止清理并保留原缓存。');
  const active = value.active === '' ? [] : [value.active];
  return { tasks: merge(prior.tasks, tasks, active),
    blockers: merge(prior.blockers, value.pending ? active : []) };
}
function purge(context: string, generation: string, includeCurrent = false) {
  const base = prefix + context + '.', legacy = oldPrefix + context;
  const keys = Object.keys(sessionStorage).filter(key => {
    const old = key === legacy || key === legacy + '.read.v1';
    const outdated = key.startsWith(base) && (includeCurrent || (key !== base + generation && key !== base + generation + '.read.v1'));
    return old || outdated;
  });
  // Validate every source and the merged capacity before deleting ANY source.
  let next = originalState(context);
  for (const key of keys)
    if (!key.endsWith('.read.v1')) next = preserve(next, sessionStorage.getItem(key));
  writeOriginals(context, next);
  for (const key of keys) sessionStorage.removeItem(key);
}
function observe(context: string, blockers: string[]) {
  if (!/^[a-f0-9]{64}$/.test(context)) throw Error('本人聊天范围未核实');
  const incoming = ids(blockers);
  if (!incoming.length) return;
  const prior = originalState(context);
  writeOriginals(context, { tasks: merge(prior.tasks, incoming), blockers: merge(prior.blockers, incoming) });
}
export type ChatHistory = {
  context: string; generation: string; key: string; marker: string;
  current: () => boolean;
  originals: () => Originals;
  retainTasks: (tasks: string[]) => void;
  retainBlockers: (tasks: string[]) => void;
  releaseBlockers: (tasks: string[]) => void;
};
function scope(context: string, generation: string): ChatHistory {
  const marker = markerPrefix + context;
  const check = () => { if (localStorage.getItem(marker) !== generation) throw new DOMException('旧聊天已清空', 'AbortError'); };
  return { context, generation, marker, key: prefix + context + '.' + generation,
    current: () => localStorage.getItem(marker) === generation,
    originals: () => originalState(context),
    retainTasks: tasks => { check(); const prior = originalState(context); writeOriginals(context, { ...prior, tasks: merge(prior.tasks, tasks) }); },
    retainBlockers: blockers => { check(); observe(context, blockers); },
    // Remove only originals for which this scan acquired matching proof.
    // A snapshot replacement could erase another newly observed blocker.
    releaseBlockers: tasks => { check(); const resolved = new Set(ids(tasks)); const prior = originalState(context);
      writeOriginals(context, { ...prior, blockers: prior.blockers.filter(id => !resolved.has(id)) }); },
  };
}
async function locked<T>(context: string, work: () => T): Promise<T> {
  if (!/^[a-f0-9]{64}$/.test(context)) throw Error('本人聊天范围未核实');
  // Serialize first-use migration and explicit clears across same-origin tabs.
  // An unsupported browser fails visibly instead of racing a global reset.
  if (!navigator.locks) throw Error('此浏览器暂不支持安全的多标签聊天清理，请使用新版浏览器。');
  return navigator.locks.request(markerPrefix + context, work);
}
export async function prepareChatHistory(context: string, observedBlockers: string[] = []): Promise<ChatHistory> {
  // Persist typed directory observations synchronously, before awaiting a Web
  // Lock, mounting controls or starting any interruptible recovery request.
  observe(context, observedBlockers);
  return locked(context, () => {
    const key = markerPrefix + context;
    let generation = localStorage.getItem(key);
    if (!generation) { generation = crypto.randomUUID(); localStorage.setItem(key, generation); }
    if (!/^[a-f0-9-]{36}$/.test(generation)) throw Error('聊天清理版本无法核验');
    if (localStorage.getItem(key) !== generation) throw Error('无法保存聊天清理版本');
    purge(context, generation);
    return scope(context, generation);
  });
}
export async function clearChatHistory(context: string): Promise<ChatHistory> {
  return locked(context, () => {
    const generation = crypto.randomUUID();
    // Preserve original IDs before discarding text. Never erase other stores.
    purge(context, generation, true);
    localStorage.setItem(markerPrefix + context, generation);
    if (localStorage.getItem(markerPrefix + context) !== generation) throw Error('无法保存聊天清理版本');
    return scope(context, generation);
  });
}
