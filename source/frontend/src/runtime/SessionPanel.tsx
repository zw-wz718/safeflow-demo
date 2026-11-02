import type { FormEvent } from 'react';
import { CircleCheck, KeyRound, LogIn, LogOut } from 'lucide-react';

export function SessionPanel({
  authenticated,
  checking,
  secret,
  busy,
  onSecretChange,
  onSignIn,
  onSignOut,
}: {
  authenticated: boolean;
  checking: boolean;
  secret: string;
  busy: string | null;
  onSecretChange: (value: string) => void;
  onSignIn: (event: FormEvent) => Promise<void>;
  onSignOut: () => Promise<void>;
}) {
  return (
    <section className="r-session" aria-labelledby="session-title">
      <div className="r-section-heading">
        <h2 id="session-title">模拟会话</h2>
        <KeyRound size={18} />
      </div>
      {authenticated ? (
        <>
          <div className="r-session-state">
            <CircleCheck size={17} />
            <span>模拟会话已登录</span>
          </div>
          <button
            className="r-command secondary"
            disabled={!!busy}
            onClick={() => void onSignOut()}
          >
            <LogOut size={16} />
            退出会话
          </button>
        </>
      ) : (
        <form onSubmit={(event) => void onSignIn(event)}>
          <label htmlFor="login-secret">模拟登录口令</label>
          <input
            id="login-secret"
            type="password"
            autoComplete="off"
            value={secret}
            disabled={!!busy || checking}
            onChange={(event) => onSecretChange(event.target.value)}
          />
          <button className="r-command primary" disabled={!secret || !!busy || checking}>
            <LogIn size={16} />
            {busy === 'login' ? '登录中' : '登录'}
          </button>
        </form>
      )}
    </section>
  );
}
