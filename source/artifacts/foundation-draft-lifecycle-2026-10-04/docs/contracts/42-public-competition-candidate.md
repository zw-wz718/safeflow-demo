# 明确公网演示候选（待指定独审）

来源：2026-10-28用户采用Pages＋Netlify＋腾讯云，固定共享既有user_001并保留操作结果，采用同设备模拟显码、评委手填、独立核验/最终确认。不是正式身份、真实银行或真实短信许可。

ApiConfig默认public_demo=None、demo_code_display=false，原本机模式保留。公网模式须显式提供精确HTTPS frontend_origin、backend_authority和trusted_proxy=127.0.0.1，API仍只绑定127.0.0.1。组合宿主启动检查public演示owner为user_001。真实连接来自唯一loopback代理，Host和X-Forwarded-Host精确等于authority、X-Forwarded-Proto唯一https、Forwarded不接受；Origin只接受固定前端，公网写请求必须有Origin，原X-SafeFlow-Request/JSON/体积/超时检查保持。Nginx只在TLS终结点重建Host/协议，Origin不重写成本机。

公网模式签发和清理会话Cookie均加Secure，仍HttpOnly/SameSite=Strict/Path=/；按原会话存储重启失效，不恢复旧确认。Netlify同源/api代理，必须实际核Cookie、Origin和响应no-store；本机转发头测试不等于已验公网TLS。

只有明确组装的bank_site且demo_code_display=true会在私有内存保存原控制能力。GET /api/demo-verification仅当前认证会话返回available。POST严格DTO：PAYMENT(task_id,operation_id,consent_id)或CARD(operation_id)，不接受owner/controller/结果/批准字段，不接受query。调用原挑战实现，原会话/操作/独立确认/限次/过期均由原后端核对；只一次返回模拟code（卡另含challenge_id），响应不缓存，不落证据/日志/浏览器存储。领取不确认、不核验、不发许可、不执行业务。

UI单独“演示验证”区域标明同设备模拟；用户领取后手填原验证表单，再点核验与最终执行。挑战最长原120秒；确认300秒、许可60秒及后台委托等原合同保持。本地显示最长120秒后清除；服务器可以因确认先过期、失败、重启等更早拒绝，以原号事实为准。无自动补码/批准、无真实短信。

本轮没有新增付款PRODUCER决议来源或无人值守调度。当前原actor需要独立私有原grant命令；缺实际已采用生产者/worker装配时付款保持PROCESSING和原占用，不能冒充最终到账。审计receiver另有已收实现，未因此启动发送器或把待投写成ACK。
