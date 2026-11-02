# 草案重启恢复适配

本批采用逐风 `e72084a809a825f24f09565d0bdc22d91e0564ab`，实际业务改动为 `dbd3f8e6a0a9ab7386f98c0dfe5d053f6f9d1692`。定点导入文件及SHA256见 `E:/银行agent/artifacts/draft-restart-adapter-2026-10-17/imports.json`；成员JSON保持原件，不将其独立SQL示例当作公共入口验证。

## 调用路径

现有 `/api/sim/draft-lifecycles` 和 `/api/agent/messages`、`/api/agent/samples` 继续使用同一 `LifecycleService`。SQL桥接开启且业务内存为空时，`observe_current` / `observe_for_action` 才调用恢复适配。没有新增公开恢复路由，客户端和模型均不能提交 `DraftRestore`。

1. 当前可信会话确定owner/task，锁定本人公共task，校验历史来源、动作摘要、公共回执和claim。未知claim先拒绝，不导入业务状态。
2. 从最后成功公共回执取得原业务草案号、业务版本、业务号及参数；搜索词取自相同payee的成功动作请求，或受信任调用入口中的本次明确输入。请求摘要和动作审计保护持久搜索来源，不从显示名、掩码推导。
3. 在调用者同一事务连接执行成员 `FundsStore::draft_for_restore`，取得原advisory锁及行锁，检查SQL当前版本、submitted、DB时间和ReadyToConfirm。进一步核对owner/task、原业务号、完整参数和原期限。
4. 调用成员 `MockDraftStore::restore_for_backend`，只恢复完整业务状态，逐字段比较返回原对象。当前会话在等待后和导入后再次检查。已有内存不会被SQL覆盖。
5. 修改/取消继续使用原select、prepare、新确认、claim和执行/结算路径。执行阶段重新核对SQL和内存，在同一事务保留业务锁直到 `save_draft`、公共结算、Outbox及COMMIT完成。恢复不导入旧确认、许可、回执缓存，不清除claim，不重调工具补库。

固定模拟生命周期创建入口使用原固定搜索词“小李”，与其实际候选查询同时保存。Agent的已有 `Selection.payee_query/payee_id` 经真实候选查询验证后传入受信任builder；仅改金额时也保存明确搜索来源。成员恢复会清空该task候选缓存，因此Agent恢复后重新查候选并检查当前会话及payee匹配，再继续规划。换收款人仍依据新的银行事实。

## 范围和拒绝

可恢复：本人、原task、与公共回执一致的完整、未提交、未过期ReadyToConfirm草案。原ID、业务版本、期限保持；修改业务版本递增，取消不递增。HTTP公共计划版本与业务版本仍分开。

无持久搜索词且无本次明确输入：返回依赖不可用，公共原回执仍可查询/重放；现有Agent选择入口或受信任后端builder可提供明确搜索词。不新建替代草案、不默认填词。NeedsInformation、终态、过期、已submitted、他人、错task/version、来源不一致均拒绝。旧会话/旧确认不能授权本次执行，须重新prepare并确认。未知claim只能查询原操作。

内存不随SQL回滚：SQL/public/Outbox/COMMIT失败后，不报告成功；原claim保留结果未知，下一次重建也不能借恢复重执。已经恢复后发现后续来源变化时不覆盖内存，要求安全处理原操作。

## 验证与剩余项

本批生成侧验证记录：`E:/银行agent/artifacts/draft-restart-adapter-2026-10-17/DELIVERY.md`。包含公共API/固定Plan Agent真实进程重启、新确认、同号重放、无搜索来源、新明确来源、拒绝条件、确定性SQL锁等待与会话撤销、并发旧SQL写阻塞到提交，以及恢复后结算失败的claim屏障。协议fixture只用于驱动已交付工具，不代表真实LLM能力。独立验收由检查侧完成。

逐风本批已确认日累计以北京时间UTC+8的00:00到次日00:00前分天。账户集合、计入状态、跨日处理中归属、规则版本、可信统计来源、共同账户仍缺；不据此开放资金或编默认统计函数。卡锁到COMMIT的一致性、外部审计映射/鉴权/发送仍按07中的具体缺口；本批不接正式资金许可、真实LLM/银行，不发布、不重打ZIP。
