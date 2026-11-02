import { useCallback, useEffect, useState, useSyncExternalStore } from 'react';
import {
  Activity,
  ArrowLeftRight,
  ArrowRight,
  Bell,
  Bot,
  ChevronDown,
  CreditCard,
  HelpCircle,
  LayoutDashboard,
  Menu,
  ReceiptText,
  Repeat2,
  ShieldCheck,
  Sparkles,
  X,
} from 'lucide-react';
import { Context } from './context';
import type { DraftPreset } from './context';
import { LocalDemoAdapter } from './domain/adapter';
import { makeFixtures } from './domain/fixtures';
import { draftStatus, formatMoney, pages } from './domain/model';
import type { Page } from './domain/model';
import { DetailRows, draftChangeLabel, Empty, IconButton, Modal, Notice, Tag } from './components';
import Overview from './pages/Overview';
import Agent from './pages/Agent';
import Transfers from './pages/Transfers';
import Bills from './pages/Bills';
import Cards from './pages/Cards';
import Subscriptions from './pages/Subscriptions';
import ActivityPage from './pages/Activity';

const adapter = new LocalDemoAdapter({
  getItem: (key) => localStorage.getItem(key),
  setItem: (key, value) => localStorage.setItem(key, value),
});
const navigation = [
  { id: 'overview', label: '银行首页', icon: LayoutDashboard },
  { id: 'agent', label: '智能助手', icon: Bot },
  { id: 'transfers', label: '转账汇款', icon: ArrowLeftRight },
  { id: 'bills', label: '我的账单', icon: ReceiptText },
  { id: 'cards', label: '银行卡', icon: CreditCard },
  { id: 'subscriptions', label: '订阅管理', icon: Repeat2 },
  { id: 'activity', label: '办理记录', icon: Activity },
] as const;
function route(): Page {
  const value = window.location.hash.slice(1);
  return pages.includes(value as Page) ? (value as Page) : 'overview';
}

export default function App() {
  const state = useSyncExternalStore(adapter.subscribe, adapter.getSnapshot);
  const [page, setPage] = useState<Page>(route);
  const [now, setNow] = useState(() => new Date());
  const [preset, setPreset] = useState<DraftPreset>();
  const [menu, setMenu] = useState(false);
  const [modal, setModal] = useState<'about' | 'profile' | 'notifications'>();
  const [warning, setWarning] = useState(adapter.recovered || adapter.storageUnavailable);
  const [toast, setToast] = useState<{ message: string; error: boolean; id: number }>();
  const fixtures = makeFixtures(now);
  const activeDrafts = state.drafts.filter((d) => draftStatus(d, now.getTime()) === 'DRAFT');
  const notify = useCallback(
    (message: string, error = false) => setToast({ message, error, id: Date.now() }),
    [],
  );
  const navigate = useCallback((next: Page, data?: DraftPreset) => {
    setPage(next);
    setPreset(data);
    setMenu(false);
    window.location.hash = next;
    window.scrollTo({ top: 0, behavior: 'instant' });
  }, []);
  useEffect(() => {
    const change = () => {
      setPage(route());
      setMenu(false);
    };
    window.addEventListener('hashchange', change);
    return () => window.removeEventListener('hashchange', change);
  }, []);
  useEffect(() => {
    const timer = setInterval(() => setNow(new Date()), 15000);
    return () => clearInterval(timer);
  }, []);
  useEffect(() => {
    if (!toast) return;
    const timer = setTimeout(() => setToast(undefined), 6000);
    return () => clearTimeout(timer);
  }, [toast]);
  useEffect(() => {
    document.title = `${navigation.find((n) => n.id === page)!.label} · SafeFlow`;
  }, [page]);
  const navBody = (
    <>
      <a
        className="brand"
        href="#overview"
        onClick={(e) => {
          e.preventDefault();
          navigate('overview');
        }}
        aria-label="SafeFlow 总览"
      >
        <span className="brand-icon">
          <ShieldCheck size={22} strokeWidth={2} />
        </span>
        <strong>
          SafeFlow<span>银行</span>
        </strong>
      </a>
      <div className="sidebar-label">个人银行服务</div>
      <nav aria-label="主导航">
        {navigation.map((item, i) => (
          <button
            className={`nav-item ${page === item.id ? 'active' : ''} ${i === 6 ? 'nav-separated' : ''}`}
            key={item.id}
            aria-label={item.label}
            title={item.label}
            onClick={() => navigate(item.id)}
            aria-current={page === item.id ? 'page' : undefined}
          >
            <item.icon size={20} strokeWidth={1.7} />
            <span>{item.label}</span>
            {item.id === 'agent' && (
              <span className="nav-spark">
                <Sparkles size={13} />
              </span>
            )}
            {item.id === 'transfers' && activeDrafts.length > 0 && (
              <span className="nav-count">{activeDrafts.length}</span>
            )}
          </button>
        ))}
      </nav>
      <div className="sidebar-bottom">
        <button
          className="nav-item help-link"
          aria-label="关于此演示"
          title="关于此演示"
          onClick={() => {
            setMenu(false);
            setModal('about');
          }}
        >
          <HelpCircle size={19} />
          <span>关于此演示</span>
        </button>
        <div className="sidebar-local">
          <span className="status-dot" />
          <span>演示环境 · 模拟数据</span>
        </div>
        <button
          className="profile-button"
          aria-label="演示用户资料"
          title="演示用户资料"
          onClick={() => {
            setMenu(false);
            setModal('profile');
          }}
        >
          <span className="avatar">E</span>
          <span>
            <strong>Echo</strong>
            <small>演示用户</small>
          </span>
          <ChevronDown size={16} />
        </button>
      </div>
    </>
  );
  return (
    <Context.Provider value={{ state, adapter, fixtures, now, page, preset, navigate, notify }}>
      <a
        href="#main-content"
        className="skip-link"
        onClick={(e) => {
          e.preventDefault();
          document.getElementById('main-content')?.focus();
        }}
      >
        跳到主内容
      </a>
      <aside className="sidebar">{navBody}</aside>
      <div className="app-main">
        <header className="topbar">
          <div className="topbar-left">
            <IconButton
              icon={Menu}
              label="打开导航"
              onClick={() => setMenu(true)}
              className="mobile-menu-button"
            />
            <span className="breadcrumb">
              SafeFlow 银行<span>/</span>
              <strong>{navigation.find((n) => n.id === page)!.label}</strong>
            </span>
            <span className="mobile-brand">SafeFlow 银行</span>
          </div>
          <div className="topbar-right">
            <button className="demo-badge" onClick={() => setModal('about')}>
              <span className="status-dot" />
              演示环境
            </button>
            <span className="topbar-divider" />
            <span className="notification-button">
              <IconButton
                icon={Bell}
                label="查看待办提醒"
                onClick={() => setModal('notifications')}
              />
              {activeDrafts.length > 0 && <span className="notification-dot" />}
            </span>
            <button
              className="topbar-avatar"
              aria-label="查看演示用户"
              title="演示用户"
              onClick={() => setModal('profile')}
            >
              E
            </button>
          </div>
        </header>
        <main id="main-content" tabIndex={-1} className="main-content">
          {warning && (
            <div className="storage-warning" role="alert">
              <span>
                {adapter.recovered
                  ? '本地数据格式损坏或版本不兼容，已恢复为空白演示状态。'
                  : '浏览器本地存储不可用，保存操作会提示失败。'}
              </span>
              <IconButton icon={X} label="关闭数据提示" onClick={() => setWarning(false)} />
            </div>
          )}
          {page === 'overview' ? (
            <Overview />
          ) : page === 'agent' ? (
            <Agent />
          ) : page === 'transfers' ? (
            <Transfers />
          ) : page === 'bills' ? (
            <Bills />
          ) : page === 'cards' ? (
            <Cards />
          ) : page === 'subscriptions' ? (
            <Subscriptions />
          ) : (
            <ActivityPage />
          )}
          <footer className="page-footer">
            <span>SafeFlow 银行</span>
            <span>演示数据 · 未连接真实银行</span>
          </footer>
        </main>
      </div>
      {menu && (
        <Modal title="银行服务" onClose={() => setMenu(false)}>
          <div className="mobile-navigation">
            {navigation.map((item) => (
              <button
                className={`nav-item ${page === item.id ? 'active' : ''}`}
                key={item.id}
                onClick={() => navigate(item.id)}
                aria-current={page === item.id ? 'page' : undefined}
              >
                <item.icon size={20} />
                <span>{item.label}</span>
                <ArrowRight size={16} />
              </button>
            ))}
          </div>
        </Modal>
      )}
      {modal === 'about' && (
        <Modal title="关于本地演示" onClose={() => setModal(undefined)}>
          <div className="about-brand">
            <ShieldCheck size={34} />
            <h3>SafeFlow 银行</h3>
            <Tag tone="green">演示环境</Tag>
          </div>
          <p className="about-copy">
            这是 SafeFlow
            银行的个人银行体验演示。账户、账单、卡片和订阅信息均为演示数据，草案与偏好保存在当前浏览器。
          </p>
          <Notice>当前未连接真实银行，页面不会执行转账、锁卡或取消商户订阅。</Notice>
          <DetailRows
            rows={[
              ['本地保存', '草案、对话、反馈、订阅偏好、操作记录'],
              ['草案有效期', '每次保存后 30 分钟有效'],
              ['真实资金操作', '暂未开通'],
            ]}
          />
          <div className="modal-actions">
            <button className="button primary" onClick={() => setModal(undefined)}>
              关闭说明
            </button>
          </div>
        </Modal>
      )}
      {modal === 'profile' && (
        <Modal title="演示用户" onClose={() => setModal(undefined)}>
          <div className="profile-detail">
            <span className="avatar large">E</span>
            <h3>Echo</h3>
            <Tag>固定演示身份</Tag>
          </div>
          <DetailRows
            rows={[
              ['身份', 'Echo · 演示用户'],
              ['数据范围', '当前浏览器中的演示数据'],
              ['真实登录', '暂未开通'],
            ]}
          />
          <div className="modal-actions">
            <button className="button secondary" onClick={() => setModal(undefined)}>
              关闭
            </button>
          </div>
        </Modal>
      )}
      {modal === 'notifications' && (
        <Modal title="提醒" onClose={() => setModal(undefined)}>
          <p className="modal-intro">待核对的转账草案与订阅提醒。</p>
          {activeDrafts.length > 0 ? (
            activeDrafts.slice(0, 5).map((d) => (
              <button
                key={d.id}
                className="notification-row"
                onClick={() => {
                  setModal(undefined);
                  navigate('transfers');
                }}
              >
                <ReceiptText size={19} />
                <span>
                  待核对草案 · ¥{formatMoney(d.amount)}
                  <small>{draftChangeLabel(d.version)} · 保存在本机</small>
                </span>
                <ArrowRight size={17} />
              </button>
            ))
          ) : (
            <Empty title="没有待核对草案" detail="保存草案后会出现在这里。" />
          )}
          <button
            className="notification-row"
            onClick={() => {
              setModal(undefined);
              navigate('subscriptions');
            }}
          >
            <Repeat2 size={19} />
            <span>
              查看订阅提醒<small>仅在本机演示中展示</small>
            </span>
            <ArrowRight size={17} />
          </button>
        </Modal>
      )}
      {toast && (
        <div
          className={`toast ${toast.error ? 'error' : ''}`}
          role={toast.error ? 'alert' : 'status'}
          key={toast.id}
        >
          <span>{toast.message}</span>
          <IconButton icon={X} label="关闭通知" onClick={() => setToast(undefined)} />
        </div>
      )}
    </Context.Provider>
  );
}
