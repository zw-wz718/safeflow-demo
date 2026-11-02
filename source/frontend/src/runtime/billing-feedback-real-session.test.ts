import { afterEach, describe, expect, it, vi } from 'vitest';
import { writeFileSync } from 'node:fs';
import { BillingFeedbackController } from './billing-feedback-controller';
import { FeedbackRecoveryStore, type RecoveryStorage } from './billing-feedback-store';

// Opt-in live test: forwards every request to the caller's OWN ephemeral API.
// No fabricated response or mocked session generation. Holds the genuine HTTP
// response until another real login replaces the cookie, then returns it intact.
const api = process.env.SAFEFLOW_BILLS_REAL_API;
const origin = process.env.SAFEFLOW_BILLS_REAL_ORIGIN;
const task = process.env.SAFEFLOW_BILLS_REAL_TASK;
const output = process.env.SAFEFLOW_BILLS_REAL_EVIDENCE;
afterEach(() => vi.unstubAllGlobals());
describe.skipIf(!api || !origin || !task || !output)(
  'actual HTTP session binding without local generation change',
  () => {
    it.each(['report', 'prepare'] as const)(
      'discards late real %s response after another valid login',
      async (stage) => {
        if (!api || !origin || !task || !output) throw Error('Explicit own environment required');
        const nativeFetch = globalThis.fetch;
        async function login() {
          const r = await nativeFetch(api + '/api/sim/session', {
            method: 'POST',
            headers: {
              'Content-Type': 'application/json',
              'X-SafeFlow-Request': '1',
              Origin: origin!,
            },
            body: JSON.stringify({ login_secret: 'isolated-http-test-secret-123456' }),
          });
          expect(r.status).toBe(200);
          const cookie = r.headers.get('set-cookie')?.split(';')[0];
          expect(cookie).toBeTruthy();
          return cookie!;
        }
        let cookie = await login();
        const firstCookie = cookie;
        let release!: () => void, reached!: () => void;
        const held = new Promise<void>((r) => {
            release = r;
          }),
          upstreamReady = new Promise<void>((r) => {
            reached = r;
          });
        let hold = stage === 'report';
        const requests: Array<{ method: string; path: string; status: number }> = [];
        vi.stubGlobal('fetch', async (input: string, init: RequestInit) => {
          const path = String(input);
          const headers = new Headers(init.headers);
          headers.set('Cookie', cookie);
          headers.set('Origin', origin!);
          const real = await nativeFetch(api + path, { ...init, headers });
          requests.push({ method: init.method ?? 'GET', path, status: real.status });
          if (
            hold &&
            (stage === 'report'
              ? path.startsWith('/api/agent/tasks/')
              : path === '/api/billing-feedback/prepare')
          ) {
            hold = false;
            reached();
            await held;
          }
          return real;
        });
        const rows = new Map<string, string>();
        const memory: RecoveryStorage = {
          get length() {
            return rows.size;
          },
          key: (i) => [...rows.keys()][i] ?? null,
          getItem: (k) => rows.get(k) ?? null,
          setItem: (k, v) => {
            rows.set(k, v);
          },
          removeItem: (k) => {
            rows.delete(k);
          },
        };
        const store = new FeedbackRecoveryStore(() => memory);
        const c = new BillingFeedbackController(() => 0, vi.fn(), vi.fn(), store);
        c.activate();
        let pending: Promise<void>;
        if (stage === 'prepare') {
          await c.load(task);
          expect(c.state.reports).toHaveLength(1);
          c.select(0, 1, 'UNSURE');
          hold = true;
          pending = c.prepare();
        } else pending = c.load(task);
        await upstreamReady;
        cookie = await login();
        expect(cookie).not.toBe(firstCookie);
        const session = await nativeFetch(api + '/api/sim/session', {
          headers: { Cookie: cookie },
        });
        expect(await session.json()).toMatchObject({ authenticated: true });
        release();
        await pending;
        expect(c.state.reports).toEqual([]);
        expect(c.state.prepared).toBeNull();
        expect(c.state.confirmed).toBe(false);
        expect(c.state.notice).toContain('服务端会话已变化');
        expect(
          requests.some((r) => r.path.endsWith('/confirm') || r.path.endsWith('/execute')),
        ).toBe(false);
        if (stage === 'prepare')
          expect(store.read()).toEqual([
            expect.objectContaining({ task_id: null, operation_id: null }),
          ]);
        else expect(store.read()).toEqual([]);
        writeFileSync(
          `${output}/real-session-${stage}.json`,
          JSON.stringify(
            {
              real_http: true,
              local_generation_unchanged: 0,
              second_session_authenticated: true,
              different_server_session: true,
              late_response_discarded: true,
              selectors: store.read(),
              requests,
            },
            null,
            2,
          ),
        );
        c.deactivate();
      },
    );
  },
);
