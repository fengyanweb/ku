# 语法

完整语法以仓库内的 `docs/syntax.md` 为准。

当前关键规则：

- `if` / `while` 条件必须是 `bool`。
- `if` / `while` / `for` 的单语句 body 可以省略 `{}`，多语句仍必须用块。
- `for` 支持数组和非负整数迭代：`for i in 10` 表示 `0..9`。
- 支持语句级 `i++`、`++i`、`i--`、`--i` 和 `+=` / `-=` / `*=` / `/=` / `%=`。
- `print(value)` 不自动换行；逐行输出使用 `println(value)`。
- 分支表达式只保留 `match`，不支持 `switch`。
- 标准库路径使用 `std.http`，不支持 `std:http`。
- HTTP 使用 `http.get/post/request`，不支持旧的 `http.try_get`。
- 箭头函数可写参数和返回类型，例如 `(a:int, b:int): int => a + b`；函数保持第一公民。
- 对象索引默认严格，缺键报错；`object[key]?` 才显式允许缺失并返回 `null`。
- 对象解构赋值支持同名、重命名、default 和 rest：`{ name, city: place, missing = fallback, ...rest } = user`。解构会消费右侧 object；需要保留原对象时显式写 `user.clone()`。
- `lib.User { ... }` 称为命名空间限定的结构体字面量。
- `http.service` / `http.server` 是函数，必须写成 `http.service()` / `http.server(config)`；旧的属性式默认对象不再兼容。
- `async fn` 调用立即启动一次性 task 句柄，必须显式返回 `T!`；`await task?` 等价于 `(await task)?`，并会消费 task。
- Ku 不提供 `task.spawn` / `Task.new` / `runtime.schedule` / `thread.spawn`；普通开发者只保存 async fn 返回的 task 并 `await`。
- runtime 默认最多 1024 个 task，blocking worker 为 `min(32, max(4, CPU 核心数))`，blocking 队列最多 1024；超限返回结构化 `task` Err。
