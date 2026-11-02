# 银行网站与月度公共接入候选

本版为2026-10-28本机待审候选。父链为第9已审范围→第10repair1待复审→第11持久模拟提醒待审→本第12；不把父候选或本版自测当正式验收。

## 实际接口

`GET /api/bank/overview` 只读当前登录模拟本人账户与SQL余额。`POST /api/bank/payment-draft` 只接受客户端先持久化的nonce，由服务端选固定本人账户/收款人和100元金额。原任务ID由nonce确定，结果未明不生成替代付款。其后复用draft-lifecycle、payment-preparations及background原入口；三次明确确认和付款专用模拟挑战不合并。producer与worker继续使用各自数据库登录/原grant，不暴露HTTP成功按钮。

本人主动冻结复用`/api/card-safety`确认、原生挑战、许可与最终事务；该宿主现在实际装配独立CardSafetyHost并受限API角色使用公共卡权限。反馈转卡的D20依赖路径仍未启用；历史反馈不授权冻结。

`POST /api/team-demo/billing`执行真实月度算法，固定as_of=2026-10-27T00:00:00+08:00。当前/上期/历史三窗分别为[10/01,10/27)、[09/01,09/27)、[07/03,10/01)。公共provider逐窗实际SQL capture，保持三个租约至算法计算、await后权限核回和capture-live检查完成；原月度算法/JSON不改。报告保存current原typed report以供反馈，不以顶层monthly report替代；原报告内容不可改写。金额保持负净额，无正基数percent为null，未知/证据不足不当安全。

订阅四写复用原公共change入口，提醒日期使用已采用固定日历，不根据流水猜云服务日期。查询模拟提醒只走本人`REMINDER_ATTEMPTS`，notification_sent恒false。页面不暴露原始JSON/授权票据/内部digest，参考号只供查原单；已提交付款准备资料明确为历史只读，当前结果由funds-result决定，终态停用再次确认、委托与撤销。

## 身份、审计与失效接缝

宿主`for_unified_bank_site`使用原生配置owner和ApiConfig模拟会话，完整账户经统一来源及SQL归属验证，不从HTTP参数或请求头构造TrustedScope。会话和账户/来源的核验保留在各原入口。当前仅本机模拟身份，不提供正式跨进程认证、生产JWT/反向代理信任或审计端ACL协议；不能把owner字符串或本机cookie直接当审计端权限。

公共同事务业务结果/事件/Outbox与原sender/严格ACK复用原已实现路径。bank_site默认audit_sender_configured=false；未配置worker时事件只待投，不报ACK成功。第9固定ef2db51接收端已有的限定范围独立保留，本包不把较新未经独审的接收器/权限checker替换进来。审计ACL/共享PG/外锚点及链存证仍需具体真实资源和已审合同，不能由本客户端新增字段模拟。

## 迁移、权限与恢复

原1—38保持，第11追加0039持久模拟提醒；本版无新SQL迁移。bank_site provision只允许新空库，在原生管理进程建立虚构三人来源/已采用消费与initial/卡目录、卡最终部署、API受限角色、billing recovery realm和必要公共权限。原API角色不取得producer/worker密码和完成器角色。显式300秒确认/60秒许可仅本机候选构造参数，不批准未定团队安全政策。

服务端持久原业务结果与绑定；客户端仅localStorage保存nonce/P/O等查询selector，写前保存失败拒绝继续。刷新/重登录只读查原号，不复活旧确认。停止宿主丢失模拟会话，重启后旧cookie拒绝，重新登录按原号查回付款/冻结/订阅/报告/反馈。未知结果不重发、不改号；现有数据库升级/共享安装未在本包启用。
