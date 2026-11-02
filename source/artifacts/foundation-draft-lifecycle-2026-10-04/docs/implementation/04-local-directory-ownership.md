# 本地目录职责检查 v1

2026-10-06，BASE-01 / BASE-R01 的工程检查子项，依据底座PRD第10节及已明确目录职责。本地工具子项已[独立验收通过](../../../ownership-check-review-2026-10-06/REVIEW.md)；不表示BASE-01父项、成员审批或业务闭环完成。

## 文件与依据

- `scripts/check-directory-ownership.mjs`：命令行检查器，仅用现有Node标准库及Git。
- `scripts/tests/check-directory-ownership.test.mjs`：自动回归，只在自动创建的临时Git仓库保存文本及测试提交，不改产品index/HEAD。
- `docs/collaboration/directory-ownership.json`：17条明确目录映射；每条有文档路径及章节。`root=workspace` 指 `E:\银行agent` 工作区中的原PRD包，`root=repository` 指产品仓库根中的既有交接文档。来源是可核对文档名称，不从来源读取或执行成员代码。

公共contracts/runtime/storage/bank-gateway/api及映射中的memory归不高兴；transfer归逐风，billing归Jonathan，cards/subscriptions归结夏，audit归风星龙，对应fixtures按任务页明确名称。已继承的 `features/transfer/` 依据现有交接中的逐风业务核心来源归逐风，不迁移或创建业务目录。计划目录可匹配职责，**不检查或证明其已经存在、获批创建或实现**。

共享 `crates/mock-bank/`、新目录、根清单/锁/工作流、docs/frontend/scripts等未明确覆盖的路径保持 `UNRESOLVED`。没有默认负责人、GitHub账号映射、CODEOWNERS或远端审批配置。新增工具自身也不会凭“底座生成”自动获得审批。

## 运行

需要现有 `node`（18及以上）与 `git`（支持 `--end-of-options` / `--path-format`，建议2.31及以上）在PATH。无需npm install、绝对工具路径或额外依赖。本轮实测Node24.14.0 / Git2.53.0.windows.3。从产品仓库根运行：

```powershell
# 必须明确根与基线；HEAD是示例，可改成希望比较的已存在commit/分支/tag。
node scripts/check-directory-ownership.mjs --repo . --base HEAD
node scripts/check-directory-ownership.mjs --repo . --base 65d6a889432b8c4815e5dab7513dccb7bc369358 --owner foundation
node --test scripts/tests/check-directory-ownership.test.mjs
```

也可从其他cwd以绝对产品仓库根调用脚本；`--repo`必须正好是Git工作树根，嵌套子目录、裸仓库、无仓库和不存在路径拒绝。不接受隐式cwd仓库。Git模式先将基线解析为commit，再并集基线→当前工作树、HEAD→暂存、暂存→工作树及非忽略未跟踪路径；即使暂存/工作树相互抵消，也保留这次改动范围。Git标准改名/复制记录包含旧新两端，删除路径也保留。此并集不是每笔历史改动的事件流，不追踪基线之前或基线后已完全恢复的历史操作。

结构化清单由交付者明确提供，例如文件内容：

```json
{
  "schemaVersion": 1,
  "paths": ["crates/runtime/example.txt", "fixtures/transfer/中文 样例.txt"]
}
```

```powershell
node scripts/check-directory-ownership.mjs --repo . --paths-json paths.json --owner foundation
```

`--base`和`--paths-json`互斥，`--owner`可省略，仅接受职责ID：foundation/transfer/bills/cards/audit。清单模式只核对提供的范围，不证明清单完整或文件存在；仍核对明确Git仓库根。清单文件路径相对调用者cwd解析，清单中的每个路径相对 `--repo` 根；清单不得带approved/授权字段。清单输入可由普通编辑器准备，工具没有写文件或改Git能力。

## 输出与退出码

标准输出只有一份JSON报告。归属已知时每个file含规范路径、ownerId/name、matchedPrefix、来源和reviewReasons；未知归属的owner及prefix为null、sources为空，不伪造来源。路径去重并按固定字符串序排列，不依赖本机locale；报告没有当前时间或随机ID。同一输入/规则产生相同分类结果。

| 退出码 | status                             | 含义                                                             |
| ------ | ---------------------------------- | ---------------------------------------------------------------- |
| 0      | CLASSIFIED / WITHIN_DECLARED_SCOPE | 已知归属，未发现未知或跨声明范围；仍需真实人工复核，不是审批通过 |
| 0      | NO_CHANGES                         | 输入没有改动；不表示工程或业务验收通过                           |
| 1      | REVIEW_REQUIRED                    | 存在归属待确认或跨声明职责；检查完成，但不能表示全通过           |
| 2      | ERROR                              | 输入/规则/Git错误；没有files分类结果，不当成无改动               |

`manualReviewScopes`列出涉及的已知职责或UNRESOLVED范围。`authorizationProvided`、`approvalVerified`、`implementationVerified`固定false；声明负责人不是登录、成员真实审批、安全授权、Permit或业务状态。Git输入报告给出解析后的baselineCommit；清单输入没有commit。不提供“已批准”开关，未启用任何CI或远端保护。

绝对路径、盘符/URI、NUL、空路径/段、`.` / `..`穿越及重复分隔符拒绝；反斜杠按目录分隔符归一化为`/`，完整且区分大小写的目录段匹配，runtime-extra不是runtime。中文、空格、tab/换行作为数据保留并在JSON转义。目录符号链接目标不读取、不遍历；分类只描述路径位置，不确认链接目标的真实职责或给予访问权。

Git命令通过参数数组执行，不启用shell，不拼接base/路径成命令。使用NUL分隔及严格UTF-8解析，保留文件名首部BOM；失败、坏输出、无效基线、截断、输出超限整体报错。关闭外部diff/textconv、fsmonitor及可选index刷新；每次调用先只读查询有效Git配置（含include来源），通过命令级配置清空所有已配置filter的clean/process/smudge并禁用required，查询失败或无法安全关闭则报错，不修改仓库配置。忽略调用者GIT_*重定向，避免悄悄换仓库或index。没有fetch/stage/commit/reset。工具只读取自身规则/明确清单，Git内部依自身diff机制读取元数据及进行改名检测；不自行读取变更文件内容，更不执行变更的业务文件。多个配置/路径读取之间仍非原子快照，应避免并发更改Git配置；本地检查工具不是恶意进程的隔离沙箱。

资源上限：输入/单条Git命令输出16MiB、清单最多100000路径、单路径4096 UTF-8字节。超限报错不截断。读取期间若工作区另有并发修改，多个只读Git命令不是原子快照；应在工作区稳定时运行，报告不可缓存为批准证据。

Git模式不支持gitlink/子模块：在任何内容diff之前，仅从index、HEAD和指定基线的NUL元数据检查160000模式，任一存在就返回ERROR / UNSUPPORTED_GITLINK / 退出码2，无files结果。禁止自动递归，diff额外禁用子模块检查；不能静默跳过内层后宣称完整检查。请分别选择子仓库明确根和基线检查，再人工核对父仓库变化。结构化清单仍只分类提供路径，不证明子模块内容完整。Git自身所有权拒绝也返回错误，工具不自动配置safe.directory或修改信任设置。

## 回归与独立验收

永久测试覆盖正常/跨界/未知、目录相似、穿越/绝对路径、中文/空格/换行、首部BOM、规则重叠/缺失来源、Git实际增删改名、暂存/未暂存/未跟踪/忽略、抵消改动、基线后提交、不同cwd、无效仓库/基线/Git失败、输入字段伪造及产品零写入边界。测试提交仅用于临时Git文本夹具，身份为明确测试占位，不配置产品作者或真实账号。

完整生成方实测、文件清单与保全见[生成报告](../../../ownership-check-generation-2026-10-06/REPORT.md)；[独立报告](../../../ownership-check-review-2026-10-06/REVIEW.md)记录29项永久、13组独立、64项前端及构建格式通过，真实工作树99条路径正确需复核，两项filter问题关闭。台账保留原pending-review交付，追加本地子项accepted记录。队员README复现仍待接收，BASE-01父项仍partial；远端审批及门禁未配置，当前Rust验收快照保持199/191/9/8。本工具不检查业务参数、会话、确认/认证、资金、审计或生产权限，不替任何成员接口实现。
