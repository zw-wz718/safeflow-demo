# 账单反馈领域审计增补：本地候选，未冻结

2026-10-25。基于原37、38与已审2026-10-23 repair1设计、Jonathan J1—J4，
仅在独立候选实现。没有活动安装、共同采用、正式领域接通或共享发送授权。

真实保存路径在原成员反馈/公共结果/任务完成事务里新增一个
`BILLING_FEEDBACK_SAVED`、私有link及0017原Outbox。三种反馈选择均仅表示保存用户反馈，
不表示欺诈判定、锁卡、认证或银行操作成功。旧raw事件及其中
`domain_audit_mapping_adopted=false`保持其原始含义，不回写成新领域采用证明。

新details精确五键：

```json
{"mapping_version":"billing-feedback-saved-v1","schema_version":1,"action":"SAVE_BILLING_FEEDBACK","result":"SAVED","lock_authorized":false}
```

公共event exact9、envelope exact3、发送请求和原ACK exact9均保持。公共随机event_id、
成员局部BIGINT事件号、接收audit_id是不同编号；receipt_ref存原接收audit_id。
occurred_at为公共audit INSERT时间，成员recorded_at为成员INSERT时间，均不称COMMIT时间。
没有selected_at，不将calculated_at当选择时间。

私有builder只在原事务连接读0025成员三表、原报告归档、原公共请求、原确认和已消费许可。
重建真实报告后逐字段核完整payload/header、Request/window/有序全部账户、复合交易、choice、
snapshot/rule/calculated_at、成员event/time、原staged、请求摘要及真实完成形态。
不反序列化成员私有类型，不假造SavedFeedback，不接用尚缺的反馈到卡reader。
link的facts_digest同时绑定完整事实及实际completed task/op；只用于内部一致性，不是外部业务证明。

0028保持0001—0027字节，追加封闭kind/exact5、source、不可变activation、逐O legacy和link。
FK、唯一性与延迟约束核双向event/result/member/source/owner/task/request/Outbox一致。
源登记仅受信显式安装，生成UUID部署身份并保存primary_identity、DB名/OID、端口、schema、成员/来源固定版本。
不提供HTTP注册，不在启动时注册或自动接受克隆。数据库owner/管理员仍是信任边界；
这些本地标记不能自动识别整个物理备份回退或保证两个克隆不并行发布。

安装前操作方须实际停止相关writer并核在途。专用安装方法取得独占advisory及相关表锁，
等待本连接协议下的writer排空，逐O登记已存在结果为legacy，再原子激活。
不按时间判断历史，不回填历史领域成功。登记不可变，也不能启用后追加legacy豁免。
同源再次安装拒绝，不用新身份重开历史。默认新库无source/activation，领域writer不写新事件。
候选没有自动停用/删除activation；停用须先停止相关writer/发送宿主并保留数据库证据。

新映射原号查询和finish都核原link/event/Outbox及完整事实；缺失/矛盾拒绝，不补写。
HTTP保存/查询结果增补`domain_audit`：`NOT_ACTIVATED`、`LEGACY_WITHOUT_DOMAIN_EVENT`、`VERIFIED_V1`。
前端严格reader兼容这三个值，旧候选缺字段也可读且不声称领域采用。
该字段不表示已外发/已ACK/已共同采用。prepare、未确认、查询不产生领域成功。
COMMIT不明只查询原O；接收/ACK失败只投原pending，不重执反馈或换O。

最后守卫在新增SQL、延迟约束之后重新核全部原账户、原确认、schema/txid、会话和期限。
事务不发送HTTP，不提前ACK。外部权限最后检查至物理COMMIT仍有原窗口，不能称跨系统原子授权。

共享sender默认关闭。旧raw pending含owner/task/O及成员stage等信息，exact5不代表全链匿名。
仍须风星龙F1—F6、所有写入口领域validator、raw出口/权限/留存/备份/客户ACL、固定双方部署版本及共同启用。
408固定版仅用于隔离兼容实验；其内部只读投影、原ACK和本地链通过不解除上述门。
