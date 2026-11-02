# 构建与本机部署

要求 Rust 1.98.1、Node 24、Python 3 和 PostgreSQL 16。Windows Rust 构建需 MSVC / Windows SDK；Linux 需相应系统编译工具。依赖由 Cargo.lock / package-lock.json 固定，首次构建需要网络或已有依赖缓存。

以下命令在仓库根目录执行。路径仅为本机示例，可换成自己的全新绝对路径。

## 构建

```powershell
rustup toolchain install 1.98.1
python source/tools/build.py build --target-dir E:/SafeFlowLocalBuild/target
cd source/frontend
npm ci --no-audit --no-fund
npm run build
```

后端输出位于目标目录的 `debug/examples/`：`bank_site`、`persistent_subscription_reminders` 和 `payment_background_actor`（Windows 带 `.exe`）。前端输出为 `source/frontend/dist/`。构建脚本只装配虚构 fixture 路径，不连接数据库或读取生产凭据。已有完整依赖缓存时可给构建脚本加 `--offline`。

## 全新本机隔离环境

Windows 提供原有功能复现工具。先阅读 [normal-feature-check.py](package-tools/normal-feature-check.py)，再运行；它会在指定的全新运行目录初始化独立 PG 集群、随机口令、模拟来源、API 和后台付款 actor，执行模拟付款和卡冻结，然后正常停止自己的进程。所有操作只在该新建本机环境，不能指向已有数据库或生产服务。

```powershell
python source/package-tools/normal-feature-check.py --name source-local-01 --runtime E:/SafeFlowLocalRun --output E:/SafeFlowLocalEvidence --binaries E:/SafeFlowLocalBuild/target/debug/examples --pg-bin "C:/Program Files/PostgreSQL/16/bin"
```

此工具继承历史固定日期/业务断言；日期变化或后续合同差异可能令断言失败。它是可阅读的正常流程示例，不是新机器、所有业务或生产验收保证。本次源码发布未重新执行其有资金模拟流程。

## 手工启动银行页

1. 使用自己新建的空 PG 库。在仓库外新建非秘密 JSON 配置：`schema`、`owner`、`bind`、`frontend_origin`、`recovery_realm`、`confirmation_seconds:300`、`permit_seconds:60`。例如 owner 为虚构 `user_001`，bind 为 `127.0.0.1:8787`，frontend_origin 为 `http://127.0.0.1:5173`；recovery_realm 使用新生成的 UUID，并在恢复原库时保持。
2. 后端工作目录为 `source/artifacts/foundation-draft-lifecycle-2026-10-04`。执行 `bank_site <配置绝对路径> provision`，只对新空库初始化；随后用同配置执行 `bank_site <配置绝对路径> serve`。
3. 两个命令均从私有 stdin 第一行读取 JSON：`database_url`、`login_secret`、`controller_secret`、`api_role`、`api_password`。provision 使用自己的新库管理账号及新 API 角色/密码；serve 使用受限 API 账号，后两字段为 null。真实值只在本机私有管道提供，不放命令行参数、Git、前端环境变量或日志。具体字段与限制以 [bank_site.rs](artifacts/foundation-draft-lifecycle-2026-10-04/crates/api/examples/bank_site.rs) 为准。
4. 前端终端执行下列命令，frontend_origin 必须精确匹配。使用自己创建的 login_secret 登录。

```powershell
cd source/frontend
$env:SAFEFLOW_BANK_API_TARGET='http://127.0.0.1:8787'
npm run dev -- --port 5173 --strictPort
```

API 私有 stdin 接收 `{"kind":"STOP"}` 后正常退出。前端用自身终端 Ctrl+C 停止。PG 用本机自有数据路径正常停止；保留同一数据库、schema 和 recovery_realm 才能恢复原号，不通过新建任务自动重付。

## 模型、验证码和独立服务

真实模型默认不配置。需要使用自己的 Command Code Key 时，在非秘密配置增加 llm.enabled、endpoint、model、timeout_seconds；实现只接受 `https://api.commandcode.ai/provider/v1/chat/completions` 和 `deepseek/deepseek-v4.1-flash`。仅 serve 的私有 stdin 增加 `llm_api_key`。Key 不进入客户端；模型不拥有最终资金授权。

本机模拟验证码由原生宿主私有挑战命令取得，见 bank_site.rs；公共演示显示需额外受控配置，不是真实短信。独立付款 actor、提醒和审计有各自入口与权限，不能把 API 启动成功当作后台已工作或审计已送达。审计接收器在 `source/references/audit96/audit-demo`，使用 Node 24 的 `npm start` 启动，具体环境项见 src/server.js。

公网部署还需要自己配置 HTTPS、可信 Origin、反向代理、最小角色、私有凭据、systemd 和备份。Netlify 前端配置见仓库 `bank/`；该目录保留早期发布版，最新完整前端以本目录 `frontend/` 为准。当前项目维护入口包含演示专用固定期限及版本守卫，不可直接套用到已有生产库或将旧期安装工具当作通用续期工具。
