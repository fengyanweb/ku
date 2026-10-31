# Native Task 的 JSON 局部值

[English](native-task-json.en.md)

native C 后端支持在 `async fn` 的局部变量中持有 `json.decode()` 返回的值。局部 JSON 可以跨 `await` 和计时器等待保存；返回、异常、取消和期限到达后的清理负责释放它。同步函数继续使用原 JSON 引擎和接口。

[完整示例](../examples/task_json.ku)解析中文、emoji、嵌套对象和数组，在真实计时器等待后取出子树，再编码为紧凑 JSON：

```sh
ku build --backend c --profile release examples/task_json.ku -o task-json
./task-json
```

Windows 目标可将输出名设为 `task-json.exe`，再运行 `.\task-json.exe`。

同一写法也可用于命名的 `async fn handler(req: HttpRequest): HttpResponse!`，经 `app.post("/json", handler)` 注册。`json.decode`、字符串键索引、`take`、`as_int`、`as_str` 和 `json.encode` 返回的 Result 继续按 `?`、`try/catch` 处理；不会自动解包。

## 所有权与预算

- `json.encode(value)` 读取原值，调用后仍可使用；`value["key"]` 返回独立拥有的值。
- `value.take("key")` 消费原根，移交所选子树；`as_int()`、`as_str()` 消费转换输入。不要在之后再次使用已消费变量。
- 构造对象时，动态 JSON 字段按已有 move 规则移入新对象。失败路径也清理已消费字段；函数、Task 和资源句柄不能充当 JSON 字段。
- 分配前预付 Task 字节预算，拥有者实际释放后才退还。该账本计算 Ku 申请的堆字节，不是进程 RSS，也不涵盖分配器内部开销或第三方库的全部内存。

解析与编码沿用原引擎：严格 JSON、深度上限 32、输入和输出各至多 1,000,000 字节。预算不足返回明确错误。单个 JSON 操作有大小限制，但没有在操作内部逐步挂起；不要将它理解为可在任意指令处抢占。

## 当前支持范围

局部动态值可保存、移交和重新编码 JSON 对象、数组、字符串、数值、布尔和 null。数组可来自解码；这不表示 native Task 已支持动态 JSON 的整数索引或异构数组字面量。对象支持字符串键索引、`take`、`as_int` 和 `as_str`。本次没有加入 KuValue 的 `.get()` 方法。

Task 参数、闭包捕获和 Task 公开返回值没有扩大到任意 JSON 值；native Task 浮点局部变量及其他未列出的操作仍受原类型子集限制。不支持的写法会在编译时拒绝。解释器、同步 C 和 LLVM 的支持范围须分别查看各自文档，不能由本页推定后端能力相同。

示例直接由 `native_task_json_test` 的永久回归读取，另有分配失败、预付预算、取消、期限、跨等待和真实 HTTP 回归。
