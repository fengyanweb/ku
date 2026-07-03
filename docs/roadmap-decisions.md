# Ku 待决策问题

这份文档只保留需要你决定的问题。已经决定并已写入实现/文档的内容不再重复堆在这里。

## 1. 函数类型是否做上下文类型推导

当前已实现：

```ku
op: fn(int, int): int = Add
loader: async fn(int): str! = Load
```

当前严格规则：赋给显式函数类型的箭头函数 / 匿名函数必须自己写清参数和返回类型。

```ku
f: fn(int): int = (x: int): int => x + 1 // 当前支持
f: fn(int): int = (x) => x + 1          // 当前不支持
```

需要你决定：

- 方案 A：保持严格，函数值表达式必须自己写类型。优点是 checker 简单、诊断稳定；缺点是用户写显式变量类型时还要再写一遍参数/返回。
- 方案 B：做上下文类型推导，让 `f: fn(int): int = (x) => x + 1` 成立。优点是好用；代价是 checker 要对函数值 body 做上下文二次检查，并处理泛型/闭包捕获/错误定位。

## 2. registry signed roots 精确 schema

已决定：官方 registry 内置根公钥；自定义 registry 必须显式配置公钥；public key 使用 `base64:`；`key_id` 使用 `ed25519:<name-or-date>`；轮换/吊销走 signed roots 文件。

仍需要你决定 roots 文件的精确字段：

- `version` 是否必须单调递增。
- `expires` 是否必填，过期后是否 fail-closed。
- `valid_keys` 是否允许多个 registry 共享一个 roots 文件。
- `revoked_keys` 是否只按 `key_id` 吊销，还是要带公钥 hash。
- `roots.json.sig` 是否由旧根签新根，还是固定由内置 root 签所有 roots。

## 3. 远程 registry 何时接入 `ku check/run`

已实现底层能力：HTTPS-only 下载、SHA-256、有限重试、内容寻址 cache、安装锁、Ed25519 index verifier、受限 `.tar.zst` 解包。

仍需要你决定启用条件：

- 方案 A：等 signed roots / custom registry config 全部完成后再接入 `ku check/run`。
- 方案 B：先只允许显式传 verifier 的实验命令，不进入普通 `ku check/run`。

## 4. unused warning 管线

已实现：

- unused import 默认 error。
- `ku check --deny-unused` 会把本地未读取变量/常量报 `E0905`。
- `_` / `_name` 表示有意未使用。

仍需要你决定 warning 输出方式：

- 方案 A：`ku check` 默认打印 warning，退出码仍为 0。
- 方案 B：只有 `--json` 输出 warning；普通文本先不打印。
- 方案 C：先不做 warning，只保留 `--deny-unused` error，等 VS Code diagnostics 支持 warning 后再开。

还需要决定：`ku build --release` 是否自动等价于 `--deny-unused`。

## 5. native target 与 runtime module matrix

已实现第一阶段 target resolver：

```txt
host
x86_64-linux
x86_64-windows
aarch64-darwin
```

C compiler 查找顺序：

```txt
KU_CC -> zig cc -> clang -> cc -> gcc
```

仍需要你决定：

- cross target 缺少可用 linker 时，是直接报错，还是自动降级到 host build。
- native C 第一阶段是否允许只支持 host，cross target 先只生成 `.c` 不链接。
- 哪些 std module 必须进入第一批 native runtime：`fs`、`time`、`json`、`http`、`config`、`task` 的优先级。

## 6. HTTP 专用响应 wire ABI

已决定：

- 普通 handler 只接收 `req`，不接收 `res/writer`。
- 普通 handler 返回 `HttpResponse` 或 `HttpResponse!`。
- writer 只通过 `http.stream(fn(writer) { ... })` 暴露。
- writer 不提供 `end()`；stream 函数正常返回后 runtime 自动结束。
- `del` 是唯一删除路由 API，对应 HTTP `DELETE`；不新增 `delete` 别名。

仍需要你决定专用响应的底层形状：

- `http.file(path)` 是否允许文本模式先落地，还是必须等二进制 body ABI 一次做对。
- `HttpResponse.body` 是否继续是 `str`，还是升级为 `str | bytes | stream`。
- `http.stream` 返回错误时，响应已开始后的日志格式和是否暴露给用户配置。
- `http.sse` 的事件对象字段是否固定为 `{ event, id, retry, data }`。
- `http.websocket` 第一阶段是否只允许 runtime 管理 upgrade，不给用户暴露底层 socket。
