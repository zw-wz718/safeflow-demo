# 本机持久模拟提醒候选

仅明确启动的当前本人会话、固定团队来源和已保存设置。不是邮件/短信/飞书送达，不是无人值守服务。没有默认账号、默认全用户扫描或默认60秒调度。

`persistent_subscription_reminders <private-config.json> <once|run|query> [attempt-ref]`复用原有限once/run/停止配置语义。配置必须给schema、owner、未存在的stop_file，以及reminder的max_messages/max_ledger_entries/scan_timeout_ms/cycles/interval_ms。范围沿既有校验：1—128条/1—4096账本项、100—30000ms、1—64轮、1000—60000ms间隔；interval必须显式配置，不表示任何共同调度政策已批准。连接和本机登录密钥只从环境取，不进argv或交付包。使用已导入、已初始化和已安装公共来源的**新隔离数据库**；不自动初始化、导入或改设置。沿用已有当前host generation，变化、退出或全部账户条件失效时结束。stop_file和Ctrl-C只停止当前有限运行，不改变原历史业务。

原ReserveScan与每条随机UUID尝试映射在同一事务保存。映射保存真实original_reminder_request_mock（完整原前缀）、实际source_position/ordinal/原消息、settings所在Journal前缀、来源/日历/完整账户/host/DB scope。原号不是owner/subscription/kind/cycle拼接，不是发送许可。预留COMMIT明确回执后，由不同backend核回并消费原非Clone batch，才允许原同步MockInAppSink回调。

渠道回调在业务事务外使用joined scoped thread：线程自己的Tokio runtime和新连接，不复用原owner锁连接/两连接writer池，不递归block_on原Tokio worker，不向独立线程排队后返回。全局剩余deadline、statement/lock timeout与stop贯穿持久渠道；线程返回前join。持久渠道只INSERT不可变本机模拟记录，按原attempt/digest幂等，另一新backend核回后才返回模拟成功。渠道COMMIT不明保留Unknown；原Journal不能保存结果时，查询显示持久渠道事实及原NEEDS_REVIEW，不复活batch补写或重新发送。

`POST /api/subscriptions`新增只读`{"kind":"REMINDER_ATTEMPTS"}`和`{"kind":"REMINDER_ATTEMPT","attempt_ref":"UUID"}`。客户端不能上传original request、消息、预留、scope或结果。响应仅当前本人完整账户权限核对后的原号、对应ordinal的原成员安全结果视图、channel_state、持久时刻和notification_sent=false；不返回正文、Journal/prefix/内部来源或DB凭据。查询不触发预留/发送/ResolveScan，native query也不启动host或改Journal。

0039仅新增两个不可变事实表及其拒改trigger。runtime最小新增权限为两个表SELECT/INSERT，不能UPDATE/DELETE/TRUNCATE。旧SQL/锁/成员状态机与原事件保持。已有最终卡部署的旧库未提供0039前向绑定，因此迁移明确拒绝此类旧安装；本候选只支持新隔离库。先迁移兼容reader再显式装writer。新提醒领域审计mapping未采用，不新增kind，不把四动作A2事件冒充发送事件。真实通道、正式调度和共享部署仍需另外采用与验收。
