# Ku 待决策问题与路线草案

这份文档是当前路线和待决策问题的唯一集中入口。已经确定的设计草案也放在这里，避免为了看后续计划来回翻多个文档。

`Ku语言总路线图.md` 保留为历史路线图和大方向参考；当前真实能力边界继续以 README、`docs/syntax.md`、版本记录和本文档为准。

## 已按选择处理

- 箭头函数：与普通函数一样可以写参数类型和返回类型；函数继续保持第一公民，可保存、赋值和调用。
- async：`async fn` 调用立即启动 task；同一函数可以启动多个 task，async main、await、blocking pool、有界 runtime、取消、等待超时和状态 API 已实现。
- 对象索引：Ku 默认严格，`object[key]` 缺键直接报错；只有显式 `object[key]?` 才允许缺失并返回 `null`。
- 命名空间结构体表述：统一称“命名空间限定的结构体类型/结构体字面量”，不创造新的结构体类别。
- async / await：第一版采用“调用即启动任务”的语义，`await` 只能在 `async fn` 内使用，native C 第一版明确拒绝 async。
- LLVM：保持文本 `.ll` 后端和清晰小子集，已按真实 IR 扩展非递归 struct 与基础/struct Result；外部 LLVM 工具只用于额外验证。
- 远程 package / registry：先做 registry manifest 和 lockfile schema，不急着接网络下载。
- native C：struct、带长度 array、enum tag/payload 和 match 已完成；下一阶段进入内存所有权、try/catch/finally 和闭包 ABI。
- match guard：继续保守，guard 分支永远不计入 enum 穷尽覆盖；当前 checker 已符合。
- HTTP service/server：保持同一种 service 对象；`http.server(config?)` 是构造别名。
- `bind/listen` 第二参数：已删除，只允许 `bind(address)` / `listen(address)`，配置来自 `http.service(config?)` / `http.server(config?)`。
- `compiled_router`：已接入真实请求匹配，不再每次请求扫描 `service.routes`。
- `listener.close()?`：已实现显式关闭，重复 close / close 后 run 会返回 Result 错误。
- 错误提示：第一批人类可读诊断已加入错误编号、note/help。
- HTTP 资源限制：同时在线连接、active/pending 有界队列、handler timeout 和 idle timeout 已执行；超限返回 503，handler 超时返回 504。
- JSON diagnostics：`ku check --json` 使用 JSON Lines，VS Code 已切到 JSON 优先、文本兼容。
- LLVM：`ku llvm file.ku` 已实现文本 `.ll` 最小后端和 golden test，不要求本机安装 LLVM。
- native C：struct、array、enum/match 第一阶段已实现，array 读写全部有界。
- registry：manifest/lockfile、精确版本/caret resolver、冲突检测和有界下载/缓存计划已实现；实际网络 I/O 尚未接入。
- LLVM：非递归 struct、字段读写和 `Result<int|bool|str|struct>` 已进入文本后端。
- async：`task.status()`、`task.cancel()`、`task.await_timeout(ms)` 已实现；等待环、深度、队列和取消均有界失败。

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
- 每个 task 有明确状态：pending / running / waiting / cancelling / completed / failed / cancelled / panicked。
- blocking stdlib 调用必须通过 blocking pool，避免阻塞 async executor。
- 超过 task 上限返回 `task/too_many_tasks`；队列满返回 `task/queue_full`，都不 panic、不无限重试。
- async task 可读取但不能修改外层捕获，checker 和 runtime 双重限制。
- self-await、await cycle 和等待深度都有界失败，避免永久等待。
- `task.await_timeout(ms)` 只限制本次等待，超时返回 `task/timeout`，不隐式取消目标任务。
- `task.cancel()` 是协作式取消。排队任务不再执行，运行中的 Ku 代码在下一安全检查点退出。
- blocking pool 中已经开始的系统调用不能强杀；取消停止等待者，但外部副作用可能自行完成。
- main 完成后会取消尚未结束的子 task，并在 1 秒有界窗口内排空；超时返回 `task/shutdown_timeout`。

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

当前仍不做：

- array / enum。
- closure。
- match。
- try / catch。
- HTTP / fs / package。
- async / await。

当前扩展：

- 非递归 struct 声明、字面量、参数/返回值和字段读写。
- `Result<int|bool|str|struct>` 的 `ok`、`fail`、`?` 和错误传播。
- CFG 目标校验，拒绝缺失/重复 block 和无条件自跳。

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

Semver / resolver 第一版：

- 解析 `major.minor.patch`。
- lockfile 固定精确版本。
- resolver 第一版只接受精确版本或简单 caret 范围。
- 同名依赖合并约束，选择满足全部约束的最高版本。
- 冲突返回 `package/dependency_conflict`，不做复杂 SAT solver 或无限回溯。

强校验：当前 `ku-fnv64-*` 只适合本地快速校验。远程包第一版应使用 `sha256-*`，lockfile 必须记录最终 checksum。

下载和缓存策略：

- 实际网络 I/O 尚未接入。
- 下载尝试次数最多 8 次；连接、读取超时和单包 100 MB 上限必须执行。
- 已验证 cache 直接复用；未命中或校验失败时下载到并发唯一的临时位置，SHA-256 通过后原子替换。
- schema、checksum mismatch 和确定性 4xx 不进入无限重试。

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

阶段 2：array，已完成第一阶段

- 第一版用 runtime-owned array 结构，不把数组退化成裸 C 指针。
- 必须保留长度，所有索引都做边界检查。
- array literal、读写 index、长度保存已完成；所有索引检查负数和上界。
- 当前 runtime-owned 内存尚无正式 free/copy/move 所有权 ABI。

阶段 3：enum / match，已完成第一阶段

- enum 使用 tag + payload layout。
- unit variant、payload variant、guard、绑定和嵌套 enum payload match 已完成。
- match lowering 复用 checker 的穷尽性结果，native 后端生成显式 tag/payload CFG。

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

### 决策 1：native 值类型的所有权模型

array 已经会分配 runtime-owned 内存，但正式后端必须决定何时复制、移动和释放；struct/enum 内嵌 array 后也依赖同一规则。

- 方案 A，推荐：默认 move，显式 `clone()`，作用域结束自动 drop。长期性能和资源边界最好，但 checker/IR 要新增 move/drop 语义。
- 方案 B：引用计数值。实现较快，复制直观，但每次复制有原子或计数成本，循环引用还要另行限制。
- 方案 C：当前阶段统一深拷贝。规则简单，但大数组和嵌套值成本高，不符合 Ku 的低资源目标。

需要你在后续进入完整 native 内存管理前选择。

### 决策 2：registry 索引与信任协议

resolver 和下载策略已经就绪，但实际联网需要确定如何发现版本和信任包：

- 方案 A，推荐：HTTPS 静态索引，每个包一个版本清单，lockfile 固定 URL + SHA-256；第一阶段不做账号体系。
- 方案 B：中心化 JSON API，支持搜索、下架和元数据更新，但服务端和兼容成本更高。
- 签名可先采用“registry 索引签名 + 包 SHA-256”，后续再加发布者签名；也可以第一版直接要求发布者签名，但工具链会明显变重。

需要你在开始真实网络下载前选择索引形式和第一版签名强度。

### 决策 3：native async ABI

当前 native C / LLVM 继续明确拒绝 async，不会偷偷退化成阻塞调用。后续可选：

- 方案 A，推荐：先完成同步 native ABI，async 继续只在解释器可用，等内存所有权和 Error ABI 稳定后设计统一 task ABI。
- 方案 B：native 每个 task 使用 OS 线程，容易落地但资源成本高，和“小协程”目标不一致。
- 方案 C：生成状态机并嵌入事件循环，语义最接近目标，但需要完整 suspension point、取消和阻塞桥接 ABI。

这项当前不阻塞同步 native 后端；进入 native async 前再选择。

## 接下来要做

1. 先定 native 值类型所有权，补 array/struct/enum 的 copy/move/drop 和无泄漏测试。
2. 完成统一 Error ABI，再做 native `try/catch/finally`、return-through-finally 和复杂 Result payload。
3. 固定闭包 ABI与捕获所有权，再做 native 闭包调用。
4. 选择 registry 索引/签名方案，接入 HTTPS 下载、SHA-256 执行、临时文件和原子 cache 更新。
5. 用真实 Ku 项目验证 native C 与 LLVM；LLVM 只扩展项目确实需要的 array/enum，不追求一次性全覆盖。
6. async 继续补压力测试和可观测性；native async 保持明确拒绝，等待 task ABI 决策。

## 语言方向

- 语法体验像 Go：简单、直接、适合写服务端。
- 语义规则像 Rust：默认严格、错误明确、少隐式、少坑。
- 运行时并发像 Go：HTTP 和 async 默认并发，用户不手写线程池。
- 资源控制像 Zig/Rust：默认有上限，不无限排队、不无限吃内存。
