# PostgreSQL 类型化单行查询

此能力位于性能开发分支，沿用 PostgreSQL 的 native C 后端范围；解释器与 LLVM 不提供数据库执行。原 `client.query(sql, [str])` 文本接口保持可用。

```ku
row = client.query_one_typed(
    "SELECT $1::int4, $2::bool, $3::text, $4::text",
    [17, true, "中文🙂", null],
    ["int4", "bool", "text", "text?"],
    ["int4", "bool", "text", "text?"]
)?
id = row.value_int(0, 0)?
enabled = row.value_bool(0, 1)?
label = row.value(0, 2)?
absent = row.is_null(0, 3)?
```

完整程序见 [pg_typed_demo.ku](../examples/pg_typed_demo.ku)。`db.conn` 是私有 libpq 连接配置，不应提交到仓库。查询读取参数数组；构造数组时仍遵循 Ku 原有的字符串所有权规则。混合字面量只在这个查询参数位置按上下文转为 `KuValue`，不代表普通混合数组声明已经支持。

## 声明与返回值

`query_one_typed(sql, params, parameter_types, result_types): PgResult!` 的四个参数依次是 SQL、`[KuValue]`、参数类型声明和结果列声明。参数声明数量必须等于参数数量，结果声明数量必须等于实际列数。类型名区分大小写：

| 声明 | Ku 参数 | PostgreSQL 二进制表示 | 读取 |
|---|---|---|---|
| `int4` | `int`，范围 -2147483648…2147483647 | INT4，4 字节网络序 | `value_int(0, col)?` |
| `int8` | 有符号 64 位 `int` | INT8，8 字节网络序 | `value_int(0, col)?` |
| `bool` | `bool` | BOOL，1 字节且只能为 0 或 1 | `value_bool(0, col)?` |
| `text` | UTF-8 `str` | TEXT | `value(0, col)?` |
| `varchar` | UTF-8 `str` | VARCHAR | `value(0, col)?` |

`?` 后缀允许 NULL，例如 `int8?`；未声明可空的列不能返回 NULL。先用 `is_null(0, col)?` 检查，再读取可空字段。NULL 与空字符串不同。文本参数拒绝内嵌 NUL；文本结果验证 UTF-8。`numeric`、浮点数、数组、域和其他未实现类型不会被猜测解码，也没有隐式文本回退。

接口要求**恰好一行**。0 行或多行返回 `pg/row_count_error`；不会仅取首行而忽略后续错误。原有 `rows()`、`cols()` 和 `is_null()` 仍可使用。类型化行的 `value_int()` 只读取 INT4/INT8，`value_bool()` 只读取 BOOL，`value()` 只读取 TEXT/VARCHAR。`value()` 继续返回新的拥有型字符串；它不是零复制视图。旧文本结果的 `value_int()` 行为保持原样。

## Task 中使用

[pg_typed_http.ku](../examples/pg_typed_http.ku) 展示 HTTP Task 绑定动态文本、整数、布尔值与 NULL。Task 参数桥支持本地、扁平的参数字面量，元素为 `int`／`bool`／`str`／`null` 标量；它不代表通用 `KuValue` Task 参数、返回值、嵌套 JSON 参数或任意动态参数数组已经支持。示例的类型声明使用固定数组。native 同步接口也能借用元素已是 `KuValue` 的现有 `[KuValue]` 数组。两个接口都不会 move 或释放输入。

## 错误与生命周期

无效参数、声明、数值范围、结果 OID、format、长度或 UTF-8 返回明确错误；主要代码为 `typed_value_error`，NULL 使用 `null_value`。声明或参数错误在 SQL 发出前被拒绝；结果错误发生时 SQL 可能已经执行。不要据此重放写入。

结果为专有的单行 owner：数值以二进制保存，字符串存储由 Ku 持有，不引用 `PGresult`。释放原结果、重置或关闭连接后，返回行仍有效。读取整数和布尔值不构造字符串，也不经过 JSON 解析。内存按实际 owner、参数副本和元数据计入现有 Task 账本；libpq 内部缓冲仍属于进程限制范围，不是逐 Task 精确计量。

获取连接、发送、读取和重置共用原绝对期限。每次查询独占一个 lease，收尽结果后执行同 lease 的 `DISCARD ALL`；不能清理或确认状态时驱逐连接。业务结果已独立保存后，重置失败仍按既有规则保留结果并驱逐连接。取消、断连和期限到达可能返回 `execution_unknown`；关闭连接不证明数据库操作已经回滚，不自动重试。

二进制参数与结果的协议规则见 [libpq 参数执行](https://www.postgresql.org/docs/16/libpq-exec.html)；结果排空见 [libpq 异步接口](https://www.postgresql.org/docs/16/libpq-async.html)。
