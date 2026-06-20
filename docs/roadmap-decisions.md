# Ku 待决策问题与路线草案

这份文档是当前路线和待决策问题的唯一集中入口。已经确定的设计草案也放在这里，避免为了看后续计划来回翻多个文档。

`Ku语言总路线图.md` 保留为历史路线图和大方向参考；当前真实能力边界继续以 README、`docs/syntax.md`、版本记录和本文档为准。

## 已按选择处理

- 箭头函数：与普通函数一样可以写参数类型和返回类型；函数继续保持第一公民，可保存、赋值和调用。
- async：`async fn` 调用立即启动 task；同一函数可以启动多个 task，async main、await、blocking pool 和有界资源 runtime 已实现。
- 对象索引：Ku 默认严格，`object[key]` 缺键直接报错；只有显式 `object[key]?` 才允许缺失并返回 `null`。
- 命名空间结构体表述：统一称“命名空间限定的结构体类型/结构体字面量”，不创造新的结构体类别。
- async / await：第一版采用“调用即启动任务”的语义，`await` 只能在 `async fn` 内使用，native C 第一版明确拒绝 async。
- LLVM：先生成文本 `.ll`，通过 golden test 锁定输出；外部 LLVM 工具只用于额外验证。
- 远程 package / registry：先做 registry manifest 和 lockfile schema，不急着接网络下载。
- native C：下一阶段按 struct、array、enum/match、try/catch/finally 的顺序推进。
- match guard：继续保守，guard 分支永远不计入 enum 穷尽覆盖；当前 checker 已符合。
- HTTP service/server：保持同一种 service 对象；`http.server(config?)` 是构造别名。
- `bind/listen` 第二参数：已删除，只允许 `bind(address)` / `listen(address)`，配置来自 `http.service(config?)` / `http.server(config?)`。
- `compiled_router`：已接入真实请求匹配，不再每次请求扫描 `service.routes`。
- `listener.close()?`：已实现显式关闭，重复 close / close 后 run 会返回 Result 错误。
- 错误提示：第一批人类可读诊断已加入错误编号、note/help。
- HTTP 资源限制：同时在线连接、active/pending 有界队列、handler timeout 和 idle timeout 已执行；超限返回 503，handler 超时返回 504。
- JSON diagnostics：`ku check --json` 使用 JSON Lines，VS Code 已切到 JSON 优先、文本兼容。
- LLVM：`ku llvm file.ku` 已实现文本 `.ll` 最小后端和 golden test，不要求本机安装 LLVM。
- native C：struct layout/literal/字段读写第一阶段已实现。
- registry：manifest 和 lockfile 离线 schema/parser 已实现严格校验；网络下载尚未实现。

## 已定路线草案

### async / await 第一版

已定选择：

- 调用 `async fn` 会立即启动任务，不采用 Rust 那种“只创建 Future、不运行”的默认语义。
- `await` 只能出现在 `async fn` 内。
- 支持 `async fn main()`，且不能和普通 `fn main()` 同时存在。
- 阻塞 stdlib 进入 blocking pool。
- native C 第一版明确拒绝 async。

第一版语义示例：

```ku
async fn load(): str! {
    res = http.get("https://example.com")?
    return ok(res.body)
}
```

`async fn` 调用返回一个 task handle。task 内部结果保留原函数返回类型，例如 `str!` 仍然是可恢复 Result。`await task?` 的含义是先等待 task 完成，再对 Result 做 `?` 传播。

`http.get`、`fs.read` 等 API 本身仍返回普通 Result；当它们在 async task 中执行时，runtime 会透明地把实际阻塞工作送入 blocking pool，因此不写 `await http.get(...)`。

Checker 边界：

- `await` outside async fn 报 `E0801`。
- `async fn main()` 和 `fn main()` 同时出现报错。
- async 函数体内允许 `?`，但仍要遵守 Result 返回类型规则。
- `ku build --native` 遇到 async/await 继续明确拒绝。

Runtime 边界：

- `max_tasks = 1024`，task 队列有界。
- blocking worker 数为 `min(32, max(4, CPU 核心数))`。
- `max_blocking_queue = 1024`。
- 每个 task 有明确完成状态：pending / ok / err / panic。
- blocking stdlib 调用必须通过 blocking pool，避免阻塞 async executor。
- 超过 task 上限返回 `task/too_many_tasks`；队列满返回 `task/queue_full`，都不 panic、不无限重试。
- async task 可读取但不能修改外层捕获，checker 和 runtime 双重限制。
- self-await、await cycle 和等待深度都有界失败，避免永久等待。

### LLVM 文本后端

已定选择：先生成文本 `.ll`，用 `llvm-as` / `lli` / `clang` 验证，并用 golden test 锁定输出。等 IR 子集稳定后，再决定是否接 `inkwell` / `llvm-sys`。

第一阶段子集：

- `int` / `bool` / `str` 字面量。
- `fn main()` 和普通函数。
- 局部变量。
- `return`。
- `if` / `while`。
- 直接函数调用。
- `print` 最小 runtime shim。

第一阶段不做：

- array / struct / enum。
- closure。
- match。
- Result / try / catch。
- HTTP / fs / package。
- async / await。

遇到非子集节点必须清楚报错，不能生成错误 `.ll`。

测试策略：

- golden test 比较 `.ll` 文本。
- 如果本机有 `llvm-as`，验证 `.ll` 可汇编。
- 如果本机有 `lli`，运行最小程序。
- 如果本机有 `clang`，验证可编译可执行。
- 外部工具缺失时跳过工具链验证，但 golden test 必须跑。

### Registry / lockfile 第一版

已定顺序：先做 registry manifest 文档草案和 lockfile schema，不急着接网络下载。

Registry manifest 草案：

```toml
name = "math"
version = "0.1.0"
source = "https://registry.example/ku/math/0.1.0.tar.gz"
checksum = "sha256-..."
```

第一版 registry manifest 只描述一个包版本，不做复杂索引协议。

Lockfile 字段草案：

```toml
[[package]]
name = "math"
version = "0.1.0"
source = "registry"
url = "https://registry.example/ku/math/0.1.0.tar.gz"
checksum = "sha256-..."
cache_key = "math-0.1.0-sha256-..."
```

Semver 第一版：

- 解析 `major.minor.patch`。
- lockfile 固定精确版本。
- resolver 第一版只接受精确版本或简单 caret 范围。
- 冲突先报错，不做复杂 SAT solver。

强校验：当前 `ku-fnv64-*` 只适合本地快速校验。远程包第一版应使用 `sha256-*`，lockfile 必须记录最终 checksum。

后续测试：

- registry manifest 解析。
- lockfile schema 写入和读取。
- bad semver 报错。
- checksum 字段格式报错。
- resolver 冲突报错。

### Native C 后端阶段计划

已定优先级：

1. struct layout / literal / field lowering。
2. array lowering。
3. enum layout / match lowering。
4. try / catch / finally native error slot。

阶段 1：struct

- 固定 struct 字段顺序，按声明顺序生成 C struct。
- struct literal 生成临时值或局部初始化。
- field read/write 映射到 C 字段访问。
- 不支持递归 struct 值，直到内存模型明确。

阶段 2：array

- 第一版用 runtime-owned array 结构，不把数组退化成裸 C 指针。
- 必须保留长度，所有索引都做边界检查。
- array literal / index / len 先做，map/filter 后做。

阶段 3：enum / match

- enum 使用 tag + payload layout。
- unit variant 先做，payload variant 后做。
- match lowering 必须复用 checker 的穷尽性结果，native 后端只负责生成分支。

阶段 4：try / catch / finally

- 完整 Error 对象 ABI 后再做。
- `?`、`fail`、`try/catch/finally` 共享同一套 error slot。
- 不允许 silent string error ABI 混进正式阶段。

后续测试：

- struct literal 和字段读写 golden C。
- array 越界返回清晰 runtime error。
- enum unit variant match。
- payload enum match。
- native 后端遇到未支持节点仍要明确拒绝。

## 仍需你决定

当前没有新的阻塞性决策。后续做到 registry 网络协议、LLVM 复杂类型或 async native lowering 的具体设计分叉时，再把需要你选择的问题集中补到这里。

## 接下来要做

1. native C 做有界 array runtime、长度保存和越界检查。
2. native C 做 enum tag/payload 和 match lowering。
3. registry 做精确版本/caret 范围解析、冲突检测，再设计网络下载和缓存更新策略。
4. LLVM 根据实际项目需要扩展 struct/Result，或继续保持清晰的小子集。
5. async 后续增加取消、超时和状态 API；native async lowering 继续明确拒绝，直到 ABI 单独决策。

## 语言方向

- 语法体验像 Go：简单、直接、适合写服务端。
- 语义规则像 Rust：默认严格、错误明确、少隐式、少坑。
- 运行时并发像 Go：HTTP 和 async 默认并发，用户不手写线程池。
- 资源控制像 Zig/Rust：默认有上限，不无限排队、不无限吃内存。
