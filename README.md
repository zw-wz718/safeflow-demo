# SafeFlow 银行 Agent

SafeFlow 将自然语言需求组织为可确认、可核查、可恢复的模拟银行办理流程。模型负责理解与规划，服务端负责可信来源、安全校验、明确确认、许可、最终事务与原号恢复。

- [项目介绍与材料下载](https://zw-wz718.github.io/safeflow-demo/)
- [评委使用指南](https://zw-wz718.github.io/safeflow-demo/guide.html)
- [银行交互站](https://candid-blini-c4fe97.netlify.app/)
- [系统架构与核心算法](https://zw-wz718.github.io/safeflow-demo/technical.html)
- [安全自评与已知风险](https://zw-wz718.github.io/safeflow-demo/security.html)

## 体验范围

支持账户查询、对话转账准备、本地订阅管理、月度账单与本人反馈、主动冻结及原号查询。服务端已接入 Command Code 模型供应商。业务使用虚构账户和模拟资金，不连接真实银行，不发送真实短信或通知，订阅取消仅记录本地意图。模型可能误解或超时，关键写操作仍需逐项确认与服务端校验。

登录方式由组织者私下提供，不在本仓库公开。评委共用虚构账户，操作记录持续保存、不自动重置。当前尾号6628卡仍冻结，网页无自助解冻入口；余额充足不能绕过冻结，当前可验证安全拒绝出账。100万元为2026年10月31日调整时的余额快照，实际余额、卡片状态及订阅候选以登录后的读取为准。

## 公开目录与构建

`pages/` 是介绍站、独立指南和三份公开PDF的发布源。`pages/content/`保存与PDF同稿的公开Markdown供构建渲染，`pages/downloads/`仅包含明确允许的PDF。`PUBLIC-MANIFEST.json`列出公开仓库文件摘要，不包含它自身，不代表任何银行业务授权。

使用Node 24.14.0，执行 `node pages/build.mjs <不存在的绝对输出目录>`。构建只复制明确列出的静态页、样式、图标和已核摘要的PDF，不复制整个源码树，不需要安装依赖。相对链接兼容GitHub Pages的`/safeflow-demo/`项目路径。正式文稿或PDF未固定、摘要不符、文件缺失时构建失败；`--preview`仅用于本地结构预览，不用于发布。

现有手动Pages工作流将构建输出发布到GitHub Pages。只有`BANK_APP_URL`为已核实的HTTPS正式origin且`BANK_APP_READY=true`时启用银行入口；缺少任一条件时入口禁用。公开说明不能代替实际入口可用性核验。

`bank/`保留已有公开前端文件，本次介绍站更新不改变其字节或部署配置；现用交互银行以独立线上版本及实际状态为准。银行使用同源`/api`代理，真实配置、数据库凭据、模型Key和控制权限由服务端私有管理，不进入本仓库或公开下载。请勿在公开issue提交口令、验证码或真实银行卡资料。
