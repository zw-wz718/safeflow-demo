import assert from 'node:assert/strict';
import test from 'node:test';

class FakeElement {
  constructor(tagName = 'div') {
    this.tagName = tagName;
    this.children = [];
    this.listeners = new Map();
    this.attributes = new Map();
    this._classes = new Set();
    this.textContent = '';
    this.disabled = false;
    this.classList = {
      add: (...names) => names.forEach((name) => this._classes.add(name)),
      remove: (...names) => names.forEach((name) => this._classes.delete(name)),
      contains: (name) => this._classes.has(name),
      toggle: (name, force) => {
        const present = force === undefined ? !this._classes.has(name) : Boolean(force);
        if (present) this._classes.add(name); else this._classes.delete(name);
        return present;
      },
    };
  }

  set className(value) { this._classes = new Set(String(value).split(/\s+/).filter(Boolean)); }
  get className() { return [...this._classes].join(' '); }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = [...nodes]; }
  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  addEventListener(name, handler) { this.listeners.set(name, handler); }
  click() { this.listeners.get('click')?.(); }
  querySelector(selector) {
    for (const child of this.children) {
      if (child.tagName === selector) return child;
      const nested = child.querySelector(selector);
      if (nested) return nested;
    }
    return null;
  }
}

const response = (status, value) => ({
  status,
  ok: status >= 200 && status < 300,
  async text() { return typeof value === 'string' ? value : JSON.stringify(value); },
});

const latestRecord = {
  audit_id: 'AUD-1', task_id: 'TASK-1', operation_id: 'OP-1',
  action: 'TRANSFER_SUCCEEDED', kind: 'TRANSFER', result: 'SUCCEEDED',
  recorded_at: '2026-10-22T06:00:00.000Z', timestamp: '2026-10-22T06:00:00.000Z',
  details: { demo_only: true },
};
const payerReceipt = {
  receipt_ref: 'DEMO-1', perspective: '模拟付款人', status: 'SUCCEEDED',
  amount: 'CNY 200.00', occurred_at: '2026-10-22T06:00:00.000Z',
  payer_label: '模拟付款人', payee_label: '模拟收款人',
};

async function openPage(recipientResponse, scenario, payerResponse = response(200, { receipt: payerReceipt })) {
  const elements = new Map();
  const document = {
    querySelector(selector) {
      if (!selector.startsWith('#')) throw new Error(`unsupported selector: ${selector}`);
      if (!elements.has(selector)) elements.set(selector, new FakeElement());
      return elements.get(selector);
    },
    createElement(tagName) { return new FakeElement(tagName); },
  };
  const oldGlobals = {
    document: globalThis.document, localStorage: globalThis.localStorage,
    fetch: globalThis.fetch, alert: globalThis.alert,
  };
  globalThis.document = document;
  globalThis.localStorage = { getItem() { return null; }, setItem() {} };
  globalThis.alert = (message) => { throw new Error(`Unexpected alert: ${message}`); };
  globalThis.fetch = async (path) => {
    if (path === '/api/audit/latest') return response(200, { records: [latestRecord] });
    if (path === '/api/verify-chain') return response(200, { valid: true, hash_version: 'v1', checked: 1 });
    if (path === '/api/receipt/OP-1?view=payer') return payerResponse;
    if (path === '/api/receipt/OP-1?view=recipient') {
      if (recipientResponse instanceof Error) throw recipientResponse;
      return recipientResponse;
    }
    throw new Error(`Unexpected request: ${path}`);
  };
  const appUrl = new URL('../public/app.js', import.meta.url);
  appUrl.searchParams.set('page-test', scenario);
  try {
    await import(appUrl.href);
    return {
      elements,
      restore() {
        for (const [key, value] of Object.entries(oldGlobals)) {
          if (value === undefined) delete globalThis[key]; else globalThis[key] = value;
        }
      },
    };
  } catch (error) {
    for (const [key, value] of Object.entries(oldGlobals)) {
      if (value === undefined) delete globalThis[key]; else globalThis[key] = value;
    }
    throw error;
  }
}

test('page shows receipt failures, hides stale payer data, and reserves empty state for known missing receipt', async () => {
  const cases = [
    ['invalid-json-200', response(200, '{"receipt":'), '回执读取失败', true],
    ['stream-disconnect-200', {
      ok: true, status: 200,
      async text() { throw new TypeError('body disconnected after 200 headers'); },
    }, '回执读取失败', true],
    ['unexpected-404', response(404, { error: 'NOT_FOUND' }), '回执读取失败', true],
    ['storage-503', response(503, { error: 'AUDIT_STORAGE_UNAVAILABLE' }), '回执读取失败', true],
    ['network', new TypeError('Failed to fetch'), '回执读取失败', true],
    ['known-missing-404', response(404, { error: 'AUDIT_NOT_FOUND', receipt: null }), '当前操作没有本地模拟转账回执', false],
  ];
  for (const [name, recipientResponse, expectedText, isError] of cases) {
    const page = await openPage(recipientResponse, name);
    try {
      const receipt = page.elements.get('#receipt');
      const notice = page.elements.get('#receipt-empty');
      assert.equal(receipt.classList.contains('hidden'), false, `${name}: payer receipt starts visible`);
      page.elements.get('#recipient-view').click();
      await new Promise((resolve) => setImmediate(resolve));
      assert.equal(receipt.classList.contains('hidden'), true, `${name}: stale payer receipt must be hidden`);
      assert.equal(notice.classList.contains('hidden'), false, `${name}: notice must be visible`);
      assert.match(notice.textContent, new RegExp(expectedText), `${name}: page feedback`);
      assert.equal(notice.classList.contains('receipt-error'), isError, `${name}: missing must differ from failure`);
      if (isError) assert.match(notice.textContent, /不能据此判断回执不存在/, `${name}: no false empty claim`);
    } finally {
      page.restore();
    }
  }
});

test('initial page load shows an invalid 200 receipt body without aborting chain display', async () => {
  const page = await openPage(response(200, { receipt: payerReceipt }), 'initial-invalid-body', response(200, '{"receipt":'));
  try {
    const receipt = page.elements.get('#receipt');
    const notice = page.elements.get('#receipt-empty');
    assert.equal(receipt.classList.contains('hidden'), true);
    assert.equal(notice.classList.contains('receipt-error'), true);
    assert.match(notice.textContent, /回执读取失败/);
    assert.match(page.elements.get('#chain').textContent, /本地链内一致/);
  } finally {
    page.restore();
  }
});
