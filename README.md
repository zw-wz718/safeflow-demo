# SafeFlow 银行 Agent

SafeFlow 将自然语言需求组织为可确认、可核查、可恢复的模拟银行办理流程。模型负责理解与规划，服务端负责可信来源、安全校验、明确确认、许可、最终事务与原号恢复。

## 完整源码与比赛材料

**[完整项目源码入口：source/](source/)**，包含 Rust 后端、React / TypeScript 前端、转账/卡片/账单/订阅模块、数据库迁移、虚构测试数据、后台付款与提醒、Node / SQLite 审计接收器及测试。

- [源码目录与模块导航](source/README.md)
- [构建及本机部署说明](source/RUNBOOK.md)
- [测试说明](source/TESTING.md)
- [本次发布检查结果](source/PUBLICATION-CHECKS.json)
- [下载整个仓库 ZIP](https://github.com/zw-wz718/safeflow-demo/archive/refs/heads/main.zip)
- [源码与部署材料正式下载](https://github.com/zw-wz718/safeflow-demo/releases/tag/source-2026-11-02-v2)

`source/` 为 2026 年 11 月 2 日从当前实际采用版本整理的完整代码快照，保留原有相对路径和代码字节。代码目录内的历史日期、candidate/proposed 名称不代表本次发布仍是该日期版本。本版已同步线上聊天请求恢复修复，限定独立审查通过；具体变化与部署边界见[SOURCE-UPDATE-20261102.md](SOURCE-UPDATE-20261102.md)。源码发布及限定审查不代表完整安全验收；此前三份固定过期准备处理仍保留原核验范围。

## 在线体验

- [项目介绍与材料下载](https://zw-wz718.github.io/safeflow-demo/)
- [评委使用指南](https://zw-wz718.github.io/safeflow-demo/guide.html)
- [银行交互站](https://candid-blini-c4fe97.netlify.app/)
- [系统架构与核心算法](https://zw-wz718.github.io/safeflow-demo/technical.html)
- [安全自评与已知风险](https://zw-wz718.github.io/safeflow-demo/security.html)

支持账户查询、对话转账准备、本地订阅管理、月度账单与本人反馈、主动冻结及原号查询。服务端已接入 Command Code 模型供应商。业务使用虚构账户和模拟资金，不连接真实银行，不发送真实短信或通知，订阅取消仅记录本地意图。模型可能误解或超时，关键写操作仍需逐项确认与服务端校验。

**演示登录口令：`DB1TwOxgqZcVMJswYWb37VwFtE1tpZp8rx116zgjRP1ymfPu`**，无需注册或填写用户名，直接在[银行交互站](https://candid-blini-c4fe97.netlify.app/)的登录框输入即可。[详细登录说明](DEMO-ACCESS.md)。评委共用虚构账户，记录持续保存、不自动重置。当前余额、卡片状态及订阅候选以登录后的读取为准；网页没有自助解冻入口。先体验转账、最后冻结卡，余额充足不能绕过有效冻结。

## 转账危险性累加

本人当日成功、当日处理中及跨日仍处理中出账，加上本笔金额，合计超过1000元时金额最低级别升为 Red，恰好1000元不因本规则升 Red。1000元是强验证门槛而非硬限额；最终还须满足已有确认、模拟码、委托、许可、余额与冻结守卫。中央软风险计算器是未绑定网站真实设备/频次信号的扩展设计，不宣传完整反欺诈或全面防拆分。

## 仓库结构

| 目录 | 内容 |
| --- | --- |
| `source/` | 最新完整项目源码、测试和本机部署说明；阅读与构建以此为准 |
| `pages/` | 已发布介绍站、指南、技术/安全材料与三份 PDF |
| `bank/` | 早期公开前端及 Netlify 配置，保留作部署结构参考；最新源码在 `source/frontend/` |

介绍站构建使用 Node 24.14.0，在根目录执行 `node pages/build.mjs <不存在的绝对输出目录>`。工作流仅手动发布明确选择的静态页面；源码公开不会自动安装到银行、变更数据库或重新发布介绍站。

`PUBLIC-MANIFEST.json` 记录本次公开文件的大小和 SHA256。真实配置、数据库密码、模型 Key、私钥、服务器管理凭据、生产数据、备份及内部聊天不随源码分发。需要运行时使用自己的私有配置与新建本机隔离数据库。保留原作者归属和现有许可标记；本次发布未另行授予开源许可证。请勿在公开 issue 提交口令、验证码或真实银行卡资料。
