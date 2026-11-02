# 测试与验证

从仓库根目录执行：

```powershell
# 检查整个 Rust 工作区所有构建目标；不执行数据库或付款测试
python source/tools/build.py check --target-dir E:/SafeFlowLocalBuild/target

# 前端正常单元测试和生产构建
cd source/frontend
npm ci --no-audit --no-fund
npm test -- src/runtime/draft-intent.test.ts src/runtime/bank-recovery.test.ts
npm run build

# 审计接收器的本机隔离测试（会启动自己的本机 HTTP / 临时 SQLite）
cd ../references/audit96/audit-demo
npm test
```

Rust 单元/集成测试在各 crate 的 src 与 tests，业务算法测试在 features 和 members，前端浏览器测试在 frontend/e2e。PG 测试和部分浏览器测试需要它们自己的隔离数据库、环境变量及已构建二进制；没有配置时可能跳过或失败，不能把跳过计为通过。不要把生产数据库 URL 或线上登录口令注入测试。

本次源码公开以文件完整性、依赖闭合、构建和选定单元测试核验为范围，实际结果见 [PUBLICATION-CHECKS.json](PUBLICATION-CHECKS.json)。不将作者历史成绩或限定独审成绩冒充本轮全量复测，不宣称全部攻击、真实银行、真实通知、生产付款、恢复演练或未来持续可用均已验证。
