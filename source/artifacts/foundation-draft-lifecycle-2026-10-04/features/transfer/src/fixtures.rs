//! 明确虚构的固定资料与测试时钟，可由底座的可信数据入口替换。

use crate::{Account, BankAccount, BankCatalog, BankGateway, Clock, GatewayError, Payee};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[derive(Default)]
pub struct MemoryBankGateway {
    users: HashMap<String, BankCatalog>,
}

impl MemoryBankGateway {
    pub fn insert(&mut self, user_id: String, catalog: BankCatalog) {
        self.users.insert(user_id, catalog);
    }
}

impl BankGateway for MemoryBankGateway {
    fn catalog(&self, user_id: &str) -> Result<BankCatalog, GatewayError> {
        self.users
            .get(user_id)
            .cloned()
            .ok_or(GatewayError::UnknownUser)
    }
}

pub fn demo_bank() -> MemoryBankGateway {
    let mut bank = MemoryBankGateway::default();
    bank.insert(
        "user_001".into(),
        BankCatalog {
            accounts: vec![BankAccount {
                account: Account {
                    id: "account_001".into(),
                    label: "模拟付款账户".into(),
                    last_four: "0001".into(),
                },
                balance: "1288.00".into(),
                currency: "CNY".into(),
            }],
            payees: vec![Payee {
                id: "payee_008".into(),
                name: "小李".into(),
                last_four: "1024".into(),
            }],
        },
    );
    bank
}

/// 只用于测试/固定演示，生产入口使用服务端 SystemClock。
#[derive(Clone)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn new(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }
    pub fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
