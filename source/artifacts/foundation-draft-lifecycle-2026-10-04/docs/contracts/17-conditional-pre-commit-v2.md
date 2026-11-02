# PRE_COMMIT 条件性门禁公共合同 v2

日期：2026-10-07。本轮实现依据用户授权的底座条件性门禁合同与缺可信来源时拒绝伪READY要求。仅合同表示、确定性校验及Runtime守卫；生成方自测交付，独立验收待检查方。

## 版本与兼容

新增 `PRE_COMMIT_SECURITY_CONTRACT_VERSION = 2`、`PreCommitSecurityRequestV2`、`PreCommitAuthenticationV2` 和独立错误 `PreCommitContractError`。从 `safeflow-contracts` 根导出，源文件 `crates/contracts/src/pre_commit.rs`。

[冻结合同06](./06-runtime-security-public-contracts.md) 的 `SECURITY_CONTRACT_VERSION = 1`、OperationSecurityRequest、SecurityContext、SecurityDecision、SecurityKernel及DomainPolicy全部结构和validate语义保留；v1 PRE_COMMIT仍要求幂等、确认、认证三个引用。v2不替换v1，不反序列化成v1，不填虚拟grant、不改成PRE_AUTH、不暗改Policy上下文。合同12的门禁存储版本2与本请求版本2是两个独立版本，未做转换或写数据库。

## 请求与校验

v2包含：

```text
contract_version、task_id、node_id、operation_id、facts_version、phase、
tool_name、plan_version、session_ref、normalized_arguments、arguments_hash、
idempotency_key、confirmation_ref、authentication
```

没有调用方指定的用户、门禁列表、可信核验布尔值或READY字段；反序列化拒绝未知字段。phase必须PRE_COMMIT，facts/plan版本必须大于0。幂等和确认引用为必填类型化引用，缺失、JSON null或空引用拒绝；arguments_hash沿用64位小写十六进制合同。请求中的参数摘要只是表示，后续可信核验必须与当前参数重新计算比较。

认证声明采用封闭的带标签枚举：

| authentication JSON                                 | 对应声明                    | 可信门禁一致性要求                                  |
| --------------------------------------------------- | --------------------------- | --------------------------------------------------- |
| `{"requirement":"NOT_REQUIRED"}`                    | 无需强验证，不允许附带grant | `["EXPLICIT_CONFIRMATION"]`                         |
| `{"requirement":"REQUIRED","auth_grant_ref":"..."}` | 需要强验证并提供引用        | `["EXPLICIT_CONFIRMATION","STRONG_AUTHENTICATION"]` |

`validate()` 仅校验请求语法；不证明声明或引用已核验。`validate_for_required_gates(gates)` 另按**实际可信裁决**的门禁检查组合：门禁必须为表中两种规范顺序，空列表、无确认、重复、反序或声明矛盾拒绝。调用方或LLM传入的列表不能作为可信来源；方法返回Ok只说明组合一致，不能签发或执行。所需门禁由已确认Profile/Policy与可信金额、累计、风险事实经实际Kernel决定，不按200元猜等级、不把所有写操作升级为强验证；未知或缺证不能解释为无需验证。

REQUIRED必须提供非空类型化引用；NOT_REQUIRED附带引用或任何`verified`字段拒绝。`sim_grant_`引用即使非空也拒绝，不能用[合同13](./13-isolated-simulation-auth-v1.md)隔离测试凭证授权正式链。经批准的项目模拟交易验证不等于合同13凭证，不要求真实短信才能设计接入；本轮没有接通该来源。

## Runtime与Kernel的拒绝边界

新增后端方法 `Runtime::evaluate_pre_commit_v2_for_backend(request)`：先做v2语法校验，解析服务端版本化会话，随后固定返回 `TRUSTED_PRE_COMMIT_VERIFICATION_UNAVAILABLE`。没有核验提供器、任意JSON成功入口、Policy调用、Tool执行或Permit构造。非法合同返回 `INVALID_PRE_COMMIT_CONTRACT`；会话不存在或不能提供版本绑定则返回原会话错误。不能用该入口取得门禁已核验的结论。

现有确定性Kernel的v1 `evaluate()`先保留原validate，再解析可信会话；PRE_COMMIT直接返回同一来源缺失错误，未进入Policy。原先“引用齐全且Policy无否决即READY”的分支删除。各安全等级和只读工具都不能绕过此守卫；PRE_TOOL/PRE_AUTH阶段与Policy上下文保持原行为。

未来恢复正式PRE_COMMIT评估须由底座另行接通可信来源，至少重新核对用户与版本化会话、任务/节点/工具、计划/事实/参数与指纹、当前Policy/Profile和风险事实、幂等绑定、明确确认的版本/期限/撤销，以及仅在门禁要求时核验本笔认证的绑定/期限/撤销。新事实、参数、计划或会话变化使旧来源失效。不得只检查引用存在，不得直接把请求声明转成验证结果；本轮没有新增这些核验器或允许任意实现返回成功的接口。

## 当前范围与验收入口

- [合同12](./12-execution-permit-storage-v1.md)的封闭VerifiedPermitSnapshot、正式request_issuance的PermitIssuanceUnavailable和数据库事务机制不改。
- 普通Runtime资金写拦截、Mock草案的独立授权、业务Tool/Policy、前端、台账及数据库均不改。
- 不新增业务、认证服务、生产资金例子或“全链跑通”声明；资料缺失的业务投影、正式身份/认证与安全签发装配仍待实际对接。

永久测试：`crates/contracts/tests/pre_commit_v2.rs`及`crates/runtime/src/pre_commit_tests.rs`。测试引用和通用调用哨兵只验证公共边界，不是认证或转账实现。实测与命令见[短交接](../handoff/SafeFlow_PRE_COMMIT条件性门禁公共合同_v0.1.md)。
