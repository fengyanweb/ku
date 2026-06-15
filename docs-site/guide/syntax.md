# 语法

完整语法以仓库内的 `docs/syntax.md` 为准。

当前关键规则：

- `if` / `while` 条件必须是 `bool`。
- 分支表达式只保留 `match`，不支持 `switch`。
- 标准库路径使用 `std.http`，不支持 `std:http`。
- HTTP 使用 `http.get/post/request`，不支持旧的 `http.try_get`。
