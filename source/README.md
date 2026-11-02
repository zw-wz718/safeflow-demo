# SafeFlow 完整项目源码

这里是 2026 年 11 月 2 日整理的银行演示项目代码，包含完整前端、Rust 后端、业务模块、数据库迁移、测试、虚构数据和审计接收器。保留源文件原有相对目录，避免破坏 Cargo 路径依赖、`include_str!` 和来源摘要。

| 内容 | 位置 |
| --- | --- |
| Rust 工作区与锁文件 | [artifacts/foundation-draft-lifecycle-2026-10-04](artifacts/foundation-draft-lifecycle-2026-10-04/) |
| HTTP API、LLM 接入与银行宿主 | [crates/api](artifacts/foundation-draft-lifecycle-2026-10-04/crates/api/) |
| 安全合同、Runtime 与模型网关 | [crates/contracts](artifacts/foundation-draft-lifecycle-2026-10-04/crates/contracts/)、[crates/runtime](artifacts/foundation-draft-lifecycle-2026-10-04/crates/runtime/) |
| PostgreSQL 存储、迁移与后台付款 | [crates/storage](artifacts/foundation-draft-lifecycle-2026-10-04/crates/storage/) |
| 转账、卡片、订阅 | [crates/transfer](artifacts/foundation-draft-lifecycle-2026-10-04/crates/transfer/)、[features](artifacts/foundation-draft-lifecycle-2026-10-04/features/) |
| 账单模块与原作者算法 | [members/jonathan](artifacts/foundation-draft-lifecycle-2026-10-04/members/jonathan/) |
| React / TypeScript 前端与测试 | [frontend](frontend/) |
| Node / SQLite 审计接收器与测试 | [references/audit96/audit-demo](references/audit96/audit-demo/) |
| 虚构测试数据 | [fixtures](fixtures/) |
| 公共接口与设计文档 | [docs/contracts](artifacts/foundation-draft-lifecycle-2026-10-04/docs/contracts/) |

运行方法见 [RUNBOOK.md](RUNBOOK.md)，测试说明见 [TESTING.md](TESTING.md)，文件摘要及来源见 [SOURCE-EXPORT.json](SOURCE-EXPORT.json)。

本目录由当前实际采用的源码快照整理。最新修订为聊天请求恢复与转账入口阻塞修复，继承此前三份固定过期准备处理、维护47和月度模拟提醒代码。最新修复已限定独审并上线，见[本版更新说明](../SOURCE-UPDATE-20261102.md)；不代表全量安全验收。

代码中的历史 `candidate`、`proposed`、日期和旧工具名称按原实现保留。目录名中的 2026-10-04 是继承的工作区名称，不代表源码停留在该日期。生产数据、服务器管理凭据、模型 Key、私钥、数据库备份、内部聊天和运行日志不随源码分发。网站共享演示登录口令经组织者授权公开，见[评委登录说明](../DEMO-ACCESS.md)。
