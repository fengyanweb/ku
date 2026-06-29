# Ku 待决策问题

这份文档只放仍需要你决定、或已经决定但必须集中追踪的大方向。已经完成并固定的语法/API 写入 `docs/syntax.md`，不在这里重复堆文档。

## 当前需要你决定

暂无。

本轮已完成的固定语法/API 和示例已写入 `docs/syntax.md`、`README.md`、`docs-site/guide/*` 与 `docs/v0.0.14.md`；这里不再重复堆清单。

## 已决定的语言方向

这些是后续设计和实现必须遵守的产品/技术取向，不需要再反复确认：

1. 语法简单参考 Go，严格规则参考 Rust，低资源和 native 布局参考 Zig / C，native 内存模型参考 Rust / C++。
2. HTTP server 默认并发，但用户不需要手写线程、不调度 task、不管理 runtime；async runtime 参考 Go / Tokio 的协作调度思路。
3. 错误提示参考 Rust / Elm；错误结果默认结构化 `Result`。
4. 包管理参考 Cargo / npm lock，但 fail-closed，签名、lockfile 和缓存一致性要更严格。
5. 数据处理性能目标参考 Rust / C++，工具链体验参考 Go。
6. `struct` / `enum` / `array` 必须走 native layout；`object` 只用于动态数据和 JSON。
7. 默认 move，显式 `clone`，自动 drop；不支持的 native 能力必须明确报错。
8. benchmark 固定覆盖 loop / array / string / JSON / HTTP / memory。

## 已决定但仍在执行队列

下面内容已经由你选择，不需要再次拍板；只是工作量较大，后续按顺序实施。

1. native closure ABI：小范围 RC env、Owned 函数值、共享绑定捕获；先做无捕获同步函数值和精确间接调用，再做 Copy 捕获，最后做 Owned 捕获。
2. native `KuString`：正式 UTF-8 `{ ptr, len, capacity, storage }` ABI，字面量 static、运行时结果 owned，`clone()` 深拷贝，move 清空源，drop 只释放 owned。
3. native dynamic object：开放寻址 hash table、Owned move/clone/drop、严格缺键错误、`object.get_or` 后续补。
4. registry fail-closed：Ed25519 detached signature、内置官方根公钥、自定义 registry 显式公钥、key rotation/revocation、受限 `.tar.zst` 解包、manifest/index/lockfile 一致性校验。
5. 真实项目验证：用两个真实 Ku 项目验证 native C / LLVM；LLVM 只按真实项目需要继续扩展。
6. native async：等 native ABI 稳定后单独设计状态机 runtime，不使用 OS 线程冒充小协程；用户侧仍只保留“async fn 返回一次性 task + await task”模型，不开放 `task.spawn`、`Task.new`、`runtime.schedule` 或 `thread.spawn`。
7. 最终 native binary build：在解释器打包型 `ku build` 稳定后，继续做完整 import graph 打包、runtime ABI lowering、object file/linker、增量 cache，并满足生成物不依赖 Ku 源码文件的验收标准。
8. 严格检查未使用 import；未使用变量/常量也进入 error 方向，但要先设计 `_` 丢弃、测试/示例豁免和跨文件导出影响。
9. 对象解构赋值按 JS 风格进入执行队列，例如 `{ code } = http`；需要先明确只支持对象字段，还是同时支持重命名/default/rest。

## 下阶段建议顺序

1. 先补 native closure 第一阶段：函数类型解析/检查、无捕获函数值 lowering、间接调用、递归深度守卫测试。
2. 再替换 native `const char*` 原型为正式 `KuString`。
3. 再做 native dynamic object 和 `object.get_or`。
4. 再做 registry 签名验证与受限 `.tar.zst` 解包。
5. 再做未使用 import/变量检查和对象解构赋值语法。
