//! 转账草案领域逻辑，临时类型不是团队公共合同，不含银行调用或授权能力。

pub mod fixtures;
mod stored;

pub use stored::{
    BankAccount, BankCatalog, BankGateway, Clock, DraftChanges, DraftDetails, DraftError,
    DraftProgress, DraftService, DraftStatus, GatewayError, StoredDraft, SystemClock,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub last_four: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payee {
    pub id: String,
    pub name: String,
    pub last_four: String,
}

/// Name 只做精确匹配，不是自然语言理解或模糊自动选人。
#[derive(Clone, Debug)]
pub enum Selection {
    Name(String),
    Id(String),
}

/// None 表示本轮未修改该字段。金额是人民币元的十进制文本。
#[derive(Default)]
pub struct DraftInput {
    pub account: Option<Selection>,
    pub payee: Option<Selection>,
    pub amount_yuan: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub task_id: String,
    pub account: Account,
    pub payee: Payee,
    pub amount_fen: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftView {
    NeedAccount(Vec<Account>),
    NeedPayee(Vec<Payee>),
    NeedAmount,
    InvalidAmount,
    /// 仅表示信息齐全，可以展示待确认草案；绝不表示通过安全检查。
    AwaitingConfirmation(Draft),
}

/// 单个内存任务。目录必须由调用方提供为当前用户可用的账户/收款人。
/// 此处不实现登录、归属校验、持久化、确认凭证、许可、幂等或扣款。
#[derive(Clone)]
pub struct DraftSession {
    task_id: String,
    accounts: Vec<Account>,
    payees: Vec<Payee>,
    account: Option<Selection>,
    payee: Option<Selection>,
    amount_yuan: Option<String>,
}

impl DraftSession {
    pub fn new(task_id: String, accounts: Vec<Account>, payees: Vec<Payee>) -> Self {
        Self {
            task_id,
            accounts,
            payees,
            account: None,
            payee: None,
            amount_yuan: None,
        }
    }

    pub fn apply(&mut self, input: DraftInput) -> DraftView {
        if input.account.is_some() {
            self.account = input.account;
        }
        if input.payee.is_some() {
            self.payee = input.payee;
        }
        if input.amount_yuan.is_some() {
            self.amount_yuan = input.amount_yuan;
        }
        self.view()
    }

    pub fn view(&self) -> DraftView {
        let account_choices: Vec<Account> = self
            .accounts
            .iter()
            .filter(|account| match &self.account {
                None => true,
                Some(Selection::Name(name)) => account.label == *name,
                Some(Selection::Id(id)) => account.id == *id,
            })
            .cloned()
            .collect();
        if self.account.is_none() || account_choices.len() != 1 {
            return DraftView::NeedAccount(account_choices);
        }

        let payee_choices: Vec<Payee> = self
            .payees
            .iter()
            .filter(|payee| match &self.payee {
                None => true,
                Some(Selection::Name(name)) => payee.name == *name,
                Some(Selection::Id(id)) => payee.id == *id,
            })
            .cloned()
            .collect();
        if self.payee.is_none() || payee_choices.len() != 1 {
            return DraftView::NeedPayee(payee_choices);
        }

        let Some(text) = &self.amount_yuan else {
            return DraftView::NeedAmount;
        };
        let Some(amount_fen) = parse_yuan(text) else {
            return DraftView::InvalidAmount;
        };
        DraftView::AwaitingConfirmation(Draft {
            task_id: self.task_id.clone(),
            account: account_choices[0].clone(),
            payee: payee_choices[0].clone(),
            amount_fen,
        })
    }
}

// 仅接受十进制元，最多两位小数；不四舍五入，不使用浮点数。
// 此处的整数分仅用于练习，接入时必须映射到团队金额合同。
fn parse_yuan(text: &str) -> Option<u64> {
    let text = text.trim();
    let mut parts = text.split('.');
    let whole = parts.next()?;
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let fractional = match parts.next() {
        None => 0,
        Some(s) if (1..=2).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()) => {
            let n = s.parse::<u64>().ok()?;
            if s.len() == 1 {
                n * 10
            } else {
                n
            }
        }
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    let amount = whole
        .parse::<u64>()
        .ok()?
        .checked_mul(100)?
        .checked_add(fractional)?;
    if amount == 0 {
        None
    } else {
        Some(amount)
    }
}
