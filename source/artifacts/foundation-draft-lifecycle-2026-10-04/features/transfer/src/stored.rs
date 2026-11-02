//! 独立草案生命周期；全部类型均为转账包临时接口，尚未接入公共安全与 Runtime。

use crate::{Account, DraftInput, DraftSession, DraftView, Payee};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BankAccount {
    pub account: Account,
    /// 只读展示资料；本包不检查可用余额，也不执行扣款。
    pub balance: String,
    pub currency: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BankCatalog {
    pub accounts: Vec<BankAccount>,
    pub payees: Vec<Payee>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayError {
    UnknownUser,
    Unavailable,
}

/// 调用方必须提供可信身份，并保证返回的目录属于该用户。没有执行银行业务的方法。
pub trait BankGateway {
    fn catalog(&self, user_id: &str) -> Result<BankCatalog, GatewayError>;
}

/// 服务端时钟，使用 Unix 秒。测试注入可控时钟，不接收用户提交的当前时间。
pub trait Clock {
    fn now(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("服务端时钟不得早于 Unix 纪元")
            .as_secs()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftStatus {
    ReadyToConfirm,
    NeedsInformation,
    Expired,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftDetails {
    pub account: Account,
    pub payee: Payee,
    /// CNY 十进制元字符串，始终规范为两位小数；内部沿用整数分。
    pub amount: String,
    pub currency: String,
    pub remark: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDraft {
    pub draft_id: String,
    pub operation_id: String,
    pub task_id: String,
    pub user_id: String,
    pub plan_version: u64,
    pub status: DraftStatus,
    pub expires_at: u64,
    /// 改成缺失、非法或多解参数时为 None，不能把上一版完整字段冒充当前字段。
    pub details: Option<DraftDetails>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftProgress {
    pub task_id: String,
    pub view: DraftView,
    /// 首次补齐之前没有草案；任务的输入仍保存在内存以供后续补充。
    pub saved: Option<StoredDraft>,
}

#[derive(Default)]
pub struct DraftChanges {
    pub input: DraftInput,
    /// 当前仅支持 CNY；非 CNY 请求拒绝，不冒充跨币种转账。
    pub currency: Option<String>,
    /// Some("") 明确清空备注；None 保留。
    pub remark: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftError {
    InvalidIdentity,
    InvalidTaskId,
    InvalidTtl,
    TimeOverflow,
    VersionOverflow,
    AccessDenied,
    NotFound,
    AlreadyCreated,
    InvalidRestore,
    VersionConflict { expected: u64, actual: u64 },
    Expired,
    Cancelled,
    NotReady,
    UnsupportedCurrency,
    Gateway(GatewayError),
}

impl std::fmt::Display for DraftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for DraftError {}

#[derive(Clone)]
struct Flow {
    user_id: String,
    session: DraftSession,
    remark: String,
    saved: Option<StoredDraft>,
}

/// 单进程内存保存。修改通过 &mut self 串行进入，不声称数据库并发或重启保证。
pub struct DraftService<G, C> {
    gateway: G,
    clock: C,
    ttl_seconds: u64,
    flows: HashMap<String, Flow>,
    draft_tasks: HashMap<String, String>,
    expired_restores: HashSet<String>,
}

impl<G: BankGateway, C: Clock> DraftService<G, C> {
    pub fn new(gateway: G, clock: C, ttl_seconds: u64) -> Result<Self, DraftError> {
        if ttl_seconds == 0 {
            return Err(DraftError::InvalidTtl);
        }
        Ok(Self {
            gateway,
            clock,
            ttl_seconds,
            flows: HashMap::new(),
            draft_tasks: HashMap::new(),
            expired_restores: HashSet::new(),
        })
    }

    /// Trusted backend import of ONE current, complete, unsubmitted SQL draft.
    /// No confirmation/permit is imported. Existing tasks are never replaced.
    pub fn restore_for_backend(
        &mut self,
        user_id: &str,
        saved: StoredDraft,
    ) -> Result<StoredDraft, DraftError> {
        if saved.user_id != user_id || user_id.trim().is_empty() {
            return Err(DraftError::AccessDenied);
        }
        if saved.task_id.trim().is_empty()
            || saved.draft_id.trim().is_empty()
            || saved.operation_id.trim().is_empty()
            || saved.plan_version == 0
        {
            return Err(DraftError::InvalidRestore);
        }
        if self.flows.contains_key(&saved.task_id) || self.draft_tasks.contains_key(&saved.draft_id)
        {
            return Err(DraftError::AlreadyCreated);
        }
        if self.flows.values().any(|flow| {
            flow.saved
                .as_ref()
                .is_some_and(|old| old.operation_id == saved.operation_id)
        }) {
            return Err(DraftError::AlreadyCreated);
        }
        check_active(&saved)?;
        if self.expired_restores.contains(&saved.draft_id) || self.clock.now() >= saved.expires_at {
            self.expired_restores.insert(saved.draft_id.clone());
            return Err(DraftError::Expired);
        }
        if saved.status != DraftStatus::ReadyToConfirm {
            return Err(DraftError::NotReady);
        }
        let data = saved.details.as_ref().ok_or(DraftError::NotReady)?;
        if data.currency != "CNY" {
            return Err(DraftError::UnsupportedCurrency);
        }
        let mut session = DraftSession::new(
            saved.task_id.clone(),
            vec![data.account.clone()],
            vec![data.payee.clone()],
        );
        let view = session.apply(DraftInput {
            account: Some(crate::Selection::Id(data.account.id.clone())),
            payee: Some(crate::Selection::Id(data.payee.id.clone())),
            amount_yuan: Some(data.amount.clone()),
        });
        if details(&view, &data.remark).as_ref() != Some(data) {
            return Err(DraftError::InvalidRestore);
        }
        let flow = Flow {
            user_id: user_id.into(),
            session,
            remark: data.remark.clone(),
            saved: Some(saved.clone()),
        };
        self.draft_tasks
            .insert(saved.draft_id.clone(), saved.task_id.clone());
        self.flows.insert(saved.task_id.clone(), flow);
        Ok(saved)
    }

    /// None 表示新任务；Some 必须来自可信调用方，沿用原任务而非另建任务。
    /// 同一任务首次补齐后用 modify，不接受 create 覆盖现有草案。
    pub fn create(
        &mut self,
        user_id: &str,
        existing_task_id: Option<&str>,
        input: DraftInput,
        remark: &str,
    ) -> Result<DraftProgress, DraftError> {
        if user_id.trim().is_empty() {
            return Err(DraftError::InvalidIdentity);
        }
        let task_id = match existing_task_id {
            Some(id) if id.trim().is_empty() => return Err(DraftError::InvalidTaskId),
            Some(id) => id.to_owned(),
            None => next_id("task"),
        };
        if let Some(flow) = self.flows.get(&task_id) {
            if flow.user_id != user_id {
                return Err(DraftError::AccessDenied);
            }
            if flow.saved.is_some() {
                return Err(DraftError::AlreadyCreated);
            }
        }
        let catalog = self.gateway.catalog(user_id).map_err(DraftError::Gateway)?;
        let accounts = catalog.accounts.into_iter().map(|a| a.account).collect();
        let mut flow = self.flows.get(&task_id).cloned().unwrap_or_else(|| Flow {
            user_id: user_id.into(),
            session: DraftSession::new(task_id.clone(), vec![], vec![]),
            remark: String::new(),
            saved: None,
        });
        flow.session.accounts = accounts;
        flow.session.payees = catalog.payees;
        flow.remark = remark.into();
        let view = flow.session.apply(input);
        if matches!(view, DraftView::AwaitingConfirmation(_)) {
            let expires_at = self
                .clock
                .now()
                .checked_add(self.ttl_seconds)
                .ok_or(DraftError::TimeOverflow)?;
            let saved = StoredDraft {
                draft_id: next_id("draft"),
                operation_id: next_id("operation"),
                task_id: task_id.clone(),
                user_id: user_id.into(),
                plan_version: 1,
                status: DraftStatus::ReadyToConfirm,
                expires_at,
                details: details(&view, &flow.remark),
            };
            self.draft_tasks
                .insert(saved.draft_id.clone(), task_id.clone());
            flow.saved = Some(saved);
        }
        let progress = DraftProgress {
            task_id: task_id.clone(),
            view,
            saved: flow.saved.clone(),
        };
        self.flows.insert(task_id, flow);
        Ok(progress)
    }

    /// 返回最新保存快照，不代表确认、授权或可以执行。
    pub fn read(&mut self, user_id: &str, draft_id: &str) -> Result<StoredDraft, DraftError> {
        let task = self.draft_tasks.get(draft_id).ok_or(DraftError::NotFound)?;
        let flow = self.flows.get_mut(task).ok_or(DraftError::NotFound)?;
        if flow.user_id != user_id {
            return Err(DraftError::AccessDenied);
        }
        let saved = flow.saved.as_mut().ok_or(DraftError::NotFound)?;
        if matches!(
            saved.status,
            DraftStatus::ReadyToConfirm | DraftStatus::NeedsInformation
        ) && self.clock.now() >= saved.expires_at
        {
            saved.status = DraftStatus::Expired;
        }
        Ok(saved.clone())
    }

    /// 仅供后续确认模块重新取得当前版本，返回值不是授权凭证或执行许可。
    pub fn current_for_review(
        &mut self,
        user_id: &str,
        draft_id: &str,
        expected_version: u64,
    ) -> Result<StoredDraft, DraftError> {
        let saved = self.read(user_id, draft_id)?;
        check_version(&saved, expected_version)?;
        check_active(&saved)?;
        if saved.status != DraftStatus::ReadyToConfirm {
            return Err(DraftError::NotReady);
        }
        Ok(saved)
    }

    /// 修改必须提交原草案号和期望版本；同一任务/操作/草案号始终保留。
    pub fn modify(
        &mut self,
        user_id: &str,
        draft_id: &str,
        expected_version: u64,
        changes: DraftChanges,
    ) -> Result<DraftProgress, DraftError> {
        let mut saved = self.read(user_id, draft_id)?;
        check_version(&saved, expected_version)?;
        check_active(&saved)?;
        if changes.currency.as_deref().is_some_and(|c| c != "CNY") {
            return Err(DraftError::UnsupportedCurrency);
        }
        let has_changes = changes.input.account.is_some()
            || changes.input.payee.is_some()
            || changes.input.amount_yuan.is_some()
            || changes.currency.is_some()
            || changes.remark.is_some();
        let mut flow = self
            .flows
            .get(&saved.task_id)
            .cloned()
            .ok_or(DraftError::NotFound)?;
        if !has_changes {
            return Ok(DraftProgress {
                task_id: saved.task_id.clone(),
                view: flow.session.view(),
                saved: Some(saved),
            });
        }
        let catalog = self.gateway.catalog(user_id).map_err(DraftError::Gateway)?;
        // 数据入口可能耗时；写回前再次检查，避免读取期间到期的草案被更新。
        saved = self.read(user_id, draft_id)?;
        check_version(&saved, expected_version)?;
        check_active(&saved)?;
        flow.session.accounts = catalog.accounts.into_iter().map(|a| a.account).collect();
        flow.session.payees = catalog.payees;
        if let Some(remark) = changes.remark {
            flow.remark = remark;
        }
        let view = flow.session.apply(changes.input);
        saved.plan_version = saved
            .plan_version
            .checked_add(1)
            .ok_or(DraftError::VersionOverflow)?;
        saved.details = details(&view, &flow.remark);
        saved.status = if saved.details.is_some() {
            DraftStatus::ReadyToConfirm
        } else {
            DraftStatus::NeedsInformation
        };
        // 有效期从首次创建起计算，修改不延长；不会默默恢复旧确认单。
        flow.saved = Some(saved.clone());
        self.flows.insert(saved.task_id.clone(), flow);
        Ok(DraftProgress {
            task_id: saved.task_id.clone(),
            view,
            saved: Some(saved),
        })
    }

    /// 本包只有未提交草案；取消不触发银行调用。相同版本重复取消返回原结果。
    pub fn cancel(
        &mut self,
        user_id: &str,
        draft_id: &str,
        expected_version: u64,
    ) -> Result<StoredDraft, DraftError> {
        let mut saved = self.read(user_id, draft_id)?;
        check_version(&saved, expected_version)?;
        if saved.status == DraftStatus::Expired {
            return Err(DraftError::Expired);
        }
        saved.status = DraftStatus::Cancelled;
        self.flows
            .get_mut(&saved.task_id)
            .ok_or(DraftError::NotFound)?
            .saved = Some(saved.clone());
        Ok(saved)
    }
}

fn check_version(saved: &StoredDraft, expected: u64) -> Result<(), DraftError> {
    if saved.plan_version != expected {
        return Err(DraftError::VersionConflict {
            expected,
            actual: saved.plan_version,
        });
    }
    Ok(())
}

fn check_active(saved: &StoredDraft) -> Result<(), DraftError> {
    match saved.status {
        DraftStatus::Expired => Err(DraftError::Expired),
        DraftStatus::Cancelled => Err(DraftError::Cancelled),
        _ => Ok(()),
    }
}

fn details(view: &DraftView, remark: &str) -> Option<DraftDetails> {
    let DraftView::AwaitingConfirmation(draft) = view else {
        return None;
    };
    Some(DraftDetails {
        account: draft.account.clone(),
        payee: draft.payee.clone(),
        amount: format!("{}.{:02}", draft.amount_fen / 100, draft.amount_fen % 100),
        currency: "CNY".into(),
        remark: remark.into(),
    })
}

fn next_id(prefix: &str) -> String {
    // 仅承诺当前进程的编号唯一；未来以底座生成的持久编号替换，不用作认证凭据。
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let sequence = loop {
        let current = SEQUENCE.load(Ordering::Relaxed);
        let next = current.checked_add(1).expect("草案编号序列耗尽");
        if SEQUENCE
            .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break current;
        }
    };
    format!("{prefix}-{}-{sequence}", std::process::id())
}
