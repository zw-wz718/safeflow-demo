# 中央风险计算与现有决策存储接入 v1

日期：2026-10-18。依据合同00及根目录《SafeFlow_自适应风险认证与安全检查规范_v0.1》2/4/5/6/7/10/12/13/14节。参数仅为 Demo 基线，不是生产银行规则。冻结 Finding/SecurityDecision 无字段或枚举改动；沿用迁移1..19。

## 可信装配和计算入口

`ToolSecurityCatalog::risk_catalog_for_backend()` 提供后端 Rust 装配入口。`enable_distinct_count_for_backend(check_id, tool, source)` 只绑定已知计数规则、适用工具和可信 Policy 来源；不接受权重、阈值、版本或客户端JSON。既有业务工具默认没有绑定，保持未接入，不新增资金工具、设备服务或业务JSON。

Kernel 对 Policy 返回的 Findings 调用唯一 `CentralPolicyCatalog::evaluate`。只有 PRE_AUTH 评分；其他阶段出现 RiskSignal 拒绝。权重、阈值、适用范围、窗口、事件去重依据及缺失策略集中在 `demo_risk_definitions()`，规则版本为规范 Demo 的 `risk-v0.1`。中央实现版本 `central-compute-v1`，模式固定 `COMPUTE_AUDIT_ONLY`。

| 规则 | 权重 | 触发 | 窗口 | 事件去重依据 | 缺失策略 | 解释器 |
|---|---:|---|---|---|---|---|
| COMMON_NEW_DEVICE_30D | 15 | 30天无成功会话 | 30天 | user+device | SKIP_WITH_AUDIT | 未启用，待设备事实 |
| COMMON_AUTH_FAILURE_10M | 20 | 2个不同失败挑战 | 10分钟 | auth_challenge_id | Fail Closed | 离散计数，可由可信后端显式绑定 |
| COMMON_HIGH_RISK_OP_10M | 20 | 3个不同高风险操作 | 10分钟 | operation_id | 强验证 | 离散计数，可显式绑定 |
| COMMON_STALE_SESSION_12H | 10 | 完整认证后超过12小时 | 当前会话 | session_id | 重新认证 | 未启用，待认证时间事实 |
| TRANSFER_FIRST_PAYEE_90D | 15 | 90天无成功转账 | 90天 | user+payee | ADD_FIXED_RISK:10 | 未启用，待业务事实 |
| TRANSFER_AMOUNT_DEVIATION_90D | 20 | 高于中位数3倍且至少500元 | 90天 | operation+facts_version | ADD_FIXED_RISK:10 | 未启用，待金额/中位数事实 |
| TRANSFER_VELOCITY_10M | 20 | 至少3个不同转账操作 | 10分钟 | operation_id | 强验证 | 离散计数，仅execute_transfer可绑定；无生产装配 |
| COMMON_RESOURCE_ENUM_10M | 15 | 至少20个不同资源 | 10分钟 | resource_id | SKIP_WITH_AUDIT | 离散计数，可显式绑定 |

以上“解释器可绑定”不等于真实数据源已接入。现有成员装配没有自动启用这些信号；公共隔离测试的 Probe 不对外注册、不执行业务，不代表真实设备、认证、频次或资金联调。

## 观察值与证据

只解释公共 Finding 合同已有的 `observed_value` 非负JSON整数，含义是规范声明的不同事件计数；不猜成员JSON对象键。它必须等于 `event_ids` 去重后的长度。每个Finding最多64个事件和4个证据引用；引用必须是最多160字节的ASCII不透明脱敏引用（字母数字及 `_.:-`），不能包含原设备/资源、会话或凭据内容。

窗口必须是合法非负毫秒区间，长度等于规则窗口。`dedup_key` 精确绑定 `operation_id:check_id:facts_version`；来源精确匹配后端绑定，版本只能是目录批准版本。同一规则只接受一份规范化观察值，重排/重复事件和证据引用不改变贡献，冲突观察值拒绝，不按调用次数产生事件。各规则只贡献一次，分数封顶100；0..19/20..39/40..69/70..100分别为Low/Medium/High/Critical。

事件真伪、实际发生时间和窗口成员资格由绑定的可信Policy/Repository负责。计算器不访问业务库，现有公共Finding未定义每个事件时间或设备/金额事实字段，因此本轮不伪造这些字段或宣称独立验证真实风险数据。正式启用需要负责人提供可核对事实接口、脱敏事件及窗口查询依据。

启用规则没有返回Finding视为缺失；显式缺失用 `observed_value=None`、原因 `MISSING_AUXILIARY_DATA`，不带伪造事件/窗口。SKIP_WITH_AUDIT保留零贡献及部分评估标记；Fail Closed或要求认证但当前路由未接入时，形成BLOCKED，不能降成0分放行。ADD_FIXED_RISK和重新认证项的观察值解释器尚未启用。

未知规则/版本、错误来源/范围/阶段、坏证据或冲突重复使中央计算失败。Kernel将其形成COMMON-17 HardDeny，原因 `CENTRAL_RISK_INVALID_OR_UNAVAILABLE`，进入同一持久决策及审计链；不持久化错误软信号原始内容，亦不伪造成功结果。

## 执行路径与未评估表示

HardDeny优先于NeedsInput；Critical最低等级BLOCKED，Red最低等级WAITING_AUTH；RequireGate不得降级这些最低要求。有效RiskSignal只贡献评分和解释，不改变执行路径、确认、最低等级和强认证门禁。Mock草案路径保留同一许可/确认核验，允许已由中央计算的软信号参与审计。PRE_COMMIT、最终Permit、付款副作用仍未启用。

冻结RiskTier只有四个数值层级，没有“未评估”枚举。本轮保留该合同：0分的Low仅表示算术区间；Kernel另写入COMMON-17 Informational，明确 `RISK_SCORING_DISABLED_NOT_ASSESSED`、`RISK_SCORING_NOT_APPLICABLE_PHASE`、`RISK_SCORING_PARTIAL_COMPUTE_AUDIT_ONLY` 或 `RISK_SCORING_COMPUTE_AUDIT_ONLY`。拒绝决策用COMMON-17 HardDeny表达未能评估，不能将任何上述状态当作真实全量风险已验证Low。

兼容原合同16条Finding上限：未配置评分的旧工具若已返回16条，则完整保留原条目，不追加第17条；装配元数据仍明确 `DISABLED_NOT_ASSESSED`。启用评分的工具必须为中央审计标记预留空间，超预算形成拒绝。

## 持久化和重放

复用 `DecisionService`、原 `security_decisions`、原审计和失效接口。只增加经过公共计数校验的持久化分支，其他任意观察值仍拒绝；记录完整贡献及原版本。保留原持久审计结构和生命周期的严格逐字段校验，通过其中的决策摘要及装配摘要追溯持久分数、贡献、目录版本与模式，不在原审计事件中附加未约定字段。

装配摘要含中央版本、全部目录参数、实际绑定内容、计算器与注册源码摘要；只有版本相同而内容不同也会改变装配摘要。现有task-first事务锁从查询首条评分快照持有到提交，串行处理同操作/事实版本/PRE_AUTH的评分请求；即使请求号不同或Runtime重建也复用原记录和期限，不重复Policy或审计。新请求号返回的是原记录的原request_id，不另建映射表或评分存储。

已有评分操作遇到装配变化，包括移除全部绑定，拒绝复用/重新定价；旧记录仍保留原摘要、配置、贡献，可按既有规则失效。需新事实版本或新操作才有新的评估生命周期。到期/撤销/失效不能通过新请求号复活。无评分绑定的旧工具继续原合同14的语义。所有合法写入者均需持有原任务锁，本轮无数据库新表或迁移。

## 可运行示例

`crates/storage/tests/decision_cases/central_scoring.rs` 的 `kernel_to_existing_store_nonzero_score_read_replay_concurrency_and_audit_example` 使用公共Finding隔离Probe：真实Kernel计算15分，调用现有DecisionService写PostgreSQL，再由另一个Runtime读回并发重放，断言一条决策、一条审计、零Permit且Operation仍为DRAFT。Probe执行函数会panic，不能作为业务成功提供器。

在配置现有Rust工具链及 `SAFEFLOW_TEST_DATABASE_URL` 后，从产品目录运行：

```powershell
cargo +1.98.1 test -p safeflow-storage --test decisions central_scoring --locked --offline -- --ignored --test-threads=1 --nocapture
cargo +1.98.1 test -p safeflow-runtime --lib risk::tests --locked --offline
cargo +1.98.1 test -p safeflow-runtime --test central_decision --locked --offline
```

本轮证据目录的 `reproduce.ps1` 提供本机既有工具链和私有数据库环境加载方式，不输出连接凭据。
