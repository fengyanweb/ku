# 语法

完整语法以仓库内的 `docs/syntax.md` 为准。

当前关键规则：

- `if` / `while` 条件必须是 `bool`。
- 分支表达式只保留 `match`，不支持 `switch`。
- 标准库路径使用 `std.http`，不支持 `std:http`。
- HTTP 使用 `http.get/post/request`，不支持旧的 `http.try_get`。
- 箭头函数可写参数和返回类型，例如 `(a:int, b:int): int => a + b`；函数保持第一公民。
- 对象索引默认严格，缺键报错；`object[key]?` 才显式允许缺失并返回 `null`。
- `lib.User { ... }` 称为命名空间限定的结构体字面量。
- `async fn` 调用立即启动 task，必须显式返回 `T!`；`await task?` 等价于 `(await task)?`。
- runtime 默认最多 1024 个 task，blocking worker 为 `min(32, max(4, CPU 核心数))`，blocking 队列最多 1024；超限返回结构化 `task` Err。
