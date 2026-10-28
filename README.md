# SafeFlow 比赛网页候选

本仓库仅包含介绍站和银行前端。银行使用虚构演示账户与模拟资金，共用账户操作结果保留，不自动重置。真实LLM尚未配置，不发送真实短信/通知，不连接真实银行。

Pages的Actions只构建pages目录。使用绝对输出路径执行`node pages/build.mjs <新输出目录>`；资源相对路径兼容`/safeflow-demo/`。只有实际验证过的Netlify正式网址才能设置为仓库变量BANK_APP_URL，未设置时按钮禁用。启用Pages须由获授权部署人员设置Actions发布来源。

Netlify项目根选择bank，Node24、原锁npm ci、dist输出。先由部署人员确认腾讯云HTTPS证书与回源，再在bank目录以非秘密SAFEFLOW_PUBLIC_API_ORIGIN环境变量运行`node configure-netlify.mjs`生成netlify.toml。值是精确HTTPS origin（无路径、认证、query或通配），未知配置直接报错；生成文件不能替代线上TLS/Set-Cookie/退出/刷新验证。Netlify部署时必须使用经核的生成配置。此变量不是VITE_*，不放控制凭据进JS。

API使用同源/api路径，配置no-store。演示登录口令只由组织者私下提供；不向源码、平台构建变量或站点写入数据库、controller或模型Key。预览域名不自动受信，后端只采用明确生产Origin。已创建Netlify项目网址为https://candid-blini-c4fe97.netlify.app；初始导入不是可用银行。Pages仍不启用体验链接。

当前候选netlify.toml采用部署侧已核TLS上游https://111.230.157.111，保留/api/:splat并置于SPA回退之前。实际Netlify生产Origin已明确为https://candid-blini-c4fe97.netlify.app，后端精确采用；真实代理行为仍待部署核验。代理26秒超时不是业务失败证明；前端8秒请求超时与后端最长30秒处理均可能先后到达，原号查询恢复保持，不自动重试写入。未来LLM长请求/流式须另核兼容性。静态headers不证明外部代理响应头，须实际检查后端no-store及平台缓存、Cookie。

Pages构建只有同时设置BANK_APP_URL和BANK_APP_READY=true才启用体验入口；后者只可在真实体验验证后设置。已知域名本身不代表体验就绪。当前保持READY未设置。

本候选已含基于确认HTTPS上游的netlify.toml，无需再次生成；生成器拒绝覆盖，改配置须在新副本进行并重新核验。
