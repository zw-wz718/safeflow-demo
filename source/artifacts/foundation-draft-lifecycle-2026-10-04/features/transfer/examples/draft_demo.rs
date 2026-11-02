use safeflow_transfer::{Account, DraftInput, DraftSession, DraftView, Payee, Selection};

fn show(view: DraftView) {
    match view {
        DraftView::NeedAccount(choices) => {
            println!("请选择付款卡（空列表表示没有匹配项）：{choices:?}")
        }
        DraftView::NeedPayee(choices) => {
            println!("请选择收款人（空列表表示没有匹配项）：{choices:?}")
        }
        DraftView::NeedAmount => println!("你要转多少钱？"),
        DraftView::InvalidAmount => {
            println!("请输入大于 0、最多两位小数且在可表示范围内的人民币金额。")
        }
        DraftView::AwaitingConfirmation(draft) => {
            println!("任务：{}", draft.task_id);
            println!(
                "付款账户：{}（尾号 {}）",
                draft.account.label, draft.account.last_four
            );
            println!(
                "收款人：{}（尾号 {}）",
                draft.payee.name, draft.payee.last_four
            );
            println!(
                "人民币 {}.{:02} 元",
                draft.amount_fen / 100,
                draft.amount_fen % 100
            );
            println!("等待确认，尚未转账。此演示不提供确认执行功能。");
        }
    }
}

fn main() {
    println!("SafeFlow 独立草案演示：虚构数据，没有银行接入。\n");
    let mut session = DraftSession::new(
        "task-demo-001".into(),
        vec![
            Account {
                id: "salary".into(),
                label: "工资卡".into(),
                last_four: "1234".into(),
            },
            Account {
                id: "savings".into(),
                label: "储蓄卡".into(),
                last_four: "5678".into(),
            },
        ],
        vec![
            Payee {
                id: "zhang".into(),
                name: "张三".into(),
                last_four: "8888".into(),
            },
            Payee {
                id: "li-a".into(),
                name: "小李".into(),
                last_four: "1111".into(),
            },
            Payee {
                id: "li-b".into(),
                name: "小李".into(),
                last_four: "2222".into(),
            },
        ],
    );
    println!("步骤一：底座已识别付款卡和张三，但尚未提供金额。");
    show(session.apply(DraftInput {
        account: Some(Selection::Name("工资卡".into())),
        payee: Some(Selection::Name("张三".into())),
        amount_yuan: None,
    }));
    println!("\n步骤二：底座补充金额 200 元。");
    show(session.apply(DraftInput {
        amount_yuan: Some("200".into()),
        ..Default::default()
    }));
}
