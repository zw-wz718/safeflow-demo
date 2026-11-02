# 操作指纹合同 v1

合同版本：`OPERATION_FINGERPRINT_VERSION = 1`

## 输入

```text
user_id
operation_id
tool_name
resource_id
normalized_arguments
plan_version
schema_version
```

可信`user_id`必须来自服务端会话。资源名称等模糊文本必须先解析为稳定ID。

## 规范化

- 对象键按字典序排列。
- 数组保留原始顺序。
- 字符串使用JSON标准转义。
- `null`、布尔值和整数使用JSON标准形式。
- 浮点数一律拒绝。
- 金额由功能Schema先规范为十进制字符串，例如`"200.00"`。
- 币种由功能Schema先规范为大写ISO代码，例如`"CNY"`。
- `plan_version`和`schema_version`必须大于0。

指纹算法：

```text
SHA-256(
  UTF-8("safeflow-operation-fingerprint:v1:")
  || UTF-8(canonical_json(payload))
)
```

## 固定测试向量

规范JSON：

```json
{"normalized_arguments":{"amount":"200.00","currency":"CNY","payee_id":"payee_008"},"operation_id":"operation_001","plan_version":1,"resource_id":"draft_001","schema_version":1,"tool_name":"execute_transfer","user_id":"user_001"}
```

预期SHA-256：

```text
de54ef29d7808fbe7711138d7bad82488d84cd0f40a058fdcb3f150b3ee3be12
```

变更用户、Tool、资源、金额、收款人、Plan版本或Schema版本必须得到不同指纹。仅改变对象键顺序不得改变指纹。

指纹只提供一致性绑定，不提供授权真实性。授权真实性由服务端Permit Store保证。

