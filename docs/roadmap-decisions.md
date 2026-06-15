# Ku 完整化前置决策文档

这份文档列出把 Ku 从当前 0.0.12 推到更完整语言时，需要先确认的设计点。目标不是拖慢实现，而是避免继续出现“语法看起来支持、实际一用就炸”的情况。

每个问题都有推荐方案。你可以直接按编号回复，例如：

```txt
1A 2B 3A 4C
其余按推荐
```

## 当前真实状态

已完成：

- VS Code import 补全不会再生成 `std.std.fs`。
- VS Code 成员补全不会再生成 `http.http.server`。
- Ku 文件默认保存格式化已开启，但格式化器仍是基础版，不是完整 prettier 级别。
- `http.get/post/request` 已返回 `{ status, headers, body }!`。
- HTTP client 默认复用连接，并有 timeout / max body 限制。
- `http.client()`、`http.text()`、`http.json()` 已有。
- `http.service` / `http.server()` 返回带默认限制的配置对象。
- `service.get/post/put/del(path, handler)` 已能注册路由到 `service.routes`。

未完成：

- 真正的 HTTP server / `listen` / 并发请求处理。
- Router 预编译匹配、params/query/header/body 懒解析。
- Ku handler ABI，也就是 Rust HTTP server 如何安全调用 Ku 函数。
- `.env` / `.yaml` 配置文件标准库。
- `async / await`、Future、Task、Executor、async main。
- async native lowering。

## 1. HTTP Server 底层模型

问题：Ku 的 HTTP server 底层到底用什么 Rust 库承载？

### A. 使用 `tiny_http` / `rouille` 类同步库先落地

优点：

- 实现快。
- 当前 Ku 解释器是同步 runtime，接入成本低。
- 可以先把 `listen`、router、request/response 对象做闭环。

缺点：

- 性能和并发模型离 hyper / Go net/http 还有距离。
- 后面做 async runtime 时可能要迁移。

### B. 使用 `hyper` / `tokio` 直接做生产方向

优点：

- 性能和生态更好。
- 更接近最终 HTTP server 架构。
- async/await 后续能复用 tokio 概念。

缺点：

- 当前 Ku runtime 是 `Rc<RefCell>` / 非 Send，同步解释器直接跨线程调用 handler 会很麻烦。
- 需要先解决 handler ABI、线程安全、共享变量模型。

### C. 先做 Ku 自己的同步 server ABI，底层实现暂时抽象

优点：

- 先固定 Ku API 和 request/response 类型。
- 后面可以把底层从同步 server 换成 hyper。

缺点：

- 第一版性能不会是最终形态。

推荐：C。先固定 Ku API 和 handler ABI，再替换底层实现。否则直接上 hyper 会被当前 runtime 所有权模型卡住。

## 2. `listen` 行为

问题：`service.listen(":8080")?` 应该阻塞当前 main，还是启动后台服务后立即返回？

### A. 阻塞式 listen

```ku
fn main() {
    service = http.service
    service.get("/", (req, res) => http.text("ok"))
    service.listen(":8080")?
}
```

优点：

- 最简单。
- 像 Go 的 `http.ListenAndServe`。
- 适合 CLI server。

缺点：

- `listen` 后面的代码默认不会执行。

### B. 后台启动，立即返回 server handle

```ku
server = service.listen(":8080")?
print("started")
server.stop()
```

优点：

- 灵活。
- 适合测试和多服务。

缺点：

- 需要 server handle、生命周期、stop/join、错误传播设计。

推荐：A 作为默认，后续增加 `service.start()` 返回 handle。简单优先。

## 3. Handler 参数顺序

你给过两种风格：`(req,res)` 和示例里有过 `(res,req)`。

### A. 固定为 `(req, res)`

```ku
service.get("/index", (req, res) => {
    return http.text("ok")
})
```

优点：

- 符合 Express、Go handler 的阅读顺序。
- 请求先进来，响应再返回。

缺点：

- 如果你更喜欢 res-first，需要调整习惯。

### B. 固定为 `(res, req)`

优点：

- 强调“要写响应”。

缺点：

- 和主流生态不一致。

推荐：A，固定 `(req, res)`。

## 4. Handler 返回值

问题：handler 是直接 `return http.text("ok")`，还是通过 `res.text("ok")` 写响应？

### A. 返回 `HttpResponse`

```ku
service.get("/", (req, res) => {
    return http.text("ok")
})
```

优点：

- 函数式、简单。
- 不需要可变 response 对象。
- 更适合当前解释器。

缺点：

- 流式 body / 分块写入要后续扩展。

### B. 使用 `res` 写响应

```ku
service.get("/", (req, res) => {
    res.text("ok")
})
```

优点：

- 类 Express。
- 后续流式写入自然。

缺点：

- 需要可变 response 对象和生命周期管理。

推荐：A。先让 handler 返回 `HttpResponse`，`res` 先保留为未来扩展，可以传但不用。

## 5. 路由参数语法

问题：路径参数用什么格式？

### A. Express 风格

```ku
service.get("/user/:id", (req, res) => {
    print(req.params.id)
})
```

### B. Go / OpenAPI 风格

```ku
service.get("/user/{id}", (req, res) => {
    print(req.params.id)
})
```

推荐：B。`{id}` 和 Ku 的结构化语法更统一，也更容易和后续文档生成、OpenAPI 对齐。

## 6. Router 匹配策略

问题：router 是简单线性扫描，还是构建预编译 trie？

### A. 先线性扫描

优点：快做。

缺点：违背你要求的“不能每次线性扫描”。

### B. 注册阶段预编译 trie

优点：

- 符合性能目标。
- `listen` 前一次性构建。

缺点：

- 实现复杂。

推荐：B。即使第一版只支持静态路径和 `{param}`，也要在 listen 前编译，不做长期线性扫描。

## 7. Request 对象字段

推荐第一版固定这些字段：

```ku
req.method: str
req.path: str
req.params: object
req.query: object
req.headers: object
req.body: str
```

待确认：

- body 第一版是否只支持 `str`？
- 是否要内置 `req.json()?`？

推荐：body 是 `str`，同时提供 `req.json()?` 或 `http.parse_json(req.body)?` 二选一。当前 Ku 还没有对象方法通用 ABI，推荐先用 `json.try_parse(req.body)?`。

## 8. Response 类型

推荐第一版：

```ku
HttpResponse {
    status: int
    headers: object
    body: str
}
```

已有：

```ku
http.text(body)
http.json(value)
```

待确认：

- `http.text("ok", 201)` 是否保留第二参数 status？
- `http.json(value, 201)` 是否同样支持？

推荐：保留，简单实用。

## 9. 共享变量和并发安全

问题：HTTP server 默认并发后，handler 捕获外层变量怎么办？

### A. 禁止 handler 修改外层变量

```ku
count = 0
service.get("/", (req, res) => {
    count = count + 1 // check 报错
})
```

优点：

- 简单稳定。
- 避免数据竞争。

缺点：

- 有状态 server 要用专门状态 API。

### B. 允许，但用锁保护

优点：灵活。

缺点：解释器和 Value 都要变成线程安全模型，改动大。

### C. 默认禁止，后续加 `state` / `atomic` / `mutex`

推荐：C。先禁止 HTTP handler 修改外层捕获变量，后续设计 `http.state()` 或 std sync 模块。

## 10. 并发模型

问题：HTTP server 默认如何并发？

### A. 线程池

优点：

- 适合同步解释器。
- 实现比 async executor 简单。

缺点：

- 每个请求跑解释器需要隔离 Env。

### B. async executor

优点：未来方向。

缺点：当前前置不够。

推荐：A 第一版。受控线程池，默认 `max_concurrency = 256`，超过后排队或返回 503。

## 11. 超时和资源限制默认值

推荐默认：

```txt
read_timeout_ms: 5000
write_timeout_ms: 5000
handler_timeout_ms: 30000
max_body_bytes: 1000000
max_header_bytes: 16384
max_connections: 1024
max_concurrency: 256
```

待确认：

- handler 超时默认 30 秒是否太长？
- 超限返回 `413/431/503` 还是 `Err`？

推荐：

- HTTP 层请求错误返回 HTTP status。
- Ku 内部启动/配置错误返回 `Err(Error)`。

## 12. `.env` / `.yaml` 配置

问题：放到哪个标准库模块？

### A. `std.config`

```ku
import "std.config"

env = config.env()
cfg = config.yaml("app.yaml")?
```

### B. 放进 `std.fs`

```ku
env = fs.env()
cfg = fs.yaml("app.yaml")?
```

推荐：A。配置不是文件 IO 本身，单独 `std.config` 更清楚。

## 13. async / await 调用语义

你已经明确不想学 Rust 那种“调用 async fn 只创建 Future 不运行”。

推荐语义：

```ku
async fn get_a(): str! {
    return http.get("https://a.com")?.body
}

fn main() {
    task = get_a()      // 立即启动任务
    body = await task?  // 等待结果
}
```

待确认：

- 调用 async fn 是否一定立即启动？推荐：是。
- `await` 是否只能在 `async fn` 里？你之前说只能在 async fn，推荐保持。
- 普通 `main` 能不能 await？推荐不能，使用 `async fn main()`。

## 14. async main

推荐：

```ku
async fn main() {
    res = await http.get("https://example.com")?
    print(res.body)
}
```

入口规则：

- `fn main()` 和 `async fn main()` 二选一。
- 不能同时存在。
- async main 的返回值规则和普通 main 一致。

## 15. `await all`

问题：语法选哪一种？

### A. 函数式

```ku
res_a, res_b = await all([get_a(), get_b()])?
```

### B. 关键字式

```ku
res_a, res_b = await all(get_a(), get_b())?
```

推荐：A。数组表达式更统一，后续可以复用 array。

## 16. Future / Task 类型是否暴露

### A. 暴露 `Task<T>`

```ku
task: Task<str> = get_a()
```

### B. 暂时不暴露类型名

```ku
task = get_a()
```

推荐：B。先让推导工作，避免泛型系统还没完全成熟时暴露太多类型。

## 17. async 错误传播

推荐：

```ku
async fn load(): str! {
    res = await http.get("https://example.com")?
    return res.body
}
```

规则：

- `await task?` 先 await，再传播 Result 错误。
- async function 返回 `T!` 时，task 的输出是 `T!`。
- `try/catch` 能捕获 await 后的 Result 错误。

## 18. async 和 native 后端

问题：native C 后端是否必须立刻支持 async？

### A. 必须支持

缺点：会拖垮 native C 后端，需要 runtime ABI、task scheduler、polling ABI。

### B. 解释器先支持，native 明确拒绝 async

推荐：B。`ku build --native` 检测到 async 直接报清楚错误。

## 19. VS Code 格式化目标

当前格式化器只是基础缩进整理。

推荐下一步：

- 保留已有保存自动格式化。
- 增加运算符空格：`a=1+2` -> `a = 1 + 2`。
- 增加逗号后空格。
- 增加 `} catch` / `} finally` 同行规则。
- 不做复杂换行，避免破坏用户代码。

待确认：

- 是否允许格式化器调整空行？
- 是否强制 4 空格？

推荐：4 空格，最多压缩连续 3 个以上空行为 1 个空行。

## 20. 需要你确认的最小清单

请优先确认这些：

1. HTTP server 底层第一版是否按“同步受控线程池 + 预编译 router”做？
2. `listen` 默认是否阻塞？
3. handler 参数是否固定 `(req, res)`？
4. handler 是否通过 `return http.text/json(...)` 返回响应？
5. 路由参数是否使用 `/user/{id}`？
6. handler 是否禁止修改外层捕获变量？
7. 配置模块是否叫 `std.config`？
8. async fn 调用是否立即启动任务？
9. `await` 是否只能出现在 `async fn`？
10. native C 后端是否先明确拒绝 async？

## 推荐的一次性实施顺序

1. 先做 HTTP server/listen 最小闭环：阻塞 listen、预编译 router、request/response 对象、资源限制、清楚错误。
2. 补 VS Code 格式化器第二阶段：运算符空格、逗号、catch/finally 规则。
3. 做 `std.config`：`.env` 和 `.yaml`。
4. 设计并实现 async parser/checker/runtime 第一版。
5. VS Code 补 async snippets / diagnostics / hover。
6. native C 后端对 HTTP server 和 async 做明确拒绝诊断，避免误导。
