# 自举

Ku 0.0.17 已实现有界 Ku Lexer、表达式 Parser 和部分模块/语句 Parser，但还不能自举完整编译器。

`bootstrap/stage1/lexer.ku` 的 `Scan(source): [Token]!` 使用 Ku 实现当前全部 73 种 token，包括单独 `&`、字符串、模板、十进制 float、注释和完整 UTF-8 byte span，不调用 Rust 的 `lexer.scan`。`&&` 保持最长匹配，`view` 仍是普通 identifier。Float 保留原始拼写，数值转换留给后续 parser。当前上限为 32768 个 Unicode 字符、4096 个 token（含 EOF）、4096 个解码字符串字符。

Lexer 差分包含 59 组固定案例、256 组固定种子输入、动态收集的 `bootstrap`/`examples` corpus，以及 20 项资源边界；精确总数以测试日志为准。native 编译包含本地 import graph，生成的 C 不含源码 runner，搬移二进制并删除源码后仍可运行。发布前基线 `11c566b` 的三系统 CI 已通过；0.0.17 以 `v0.0.17` tag 同一提交的实际三系统 CI 结果为准，不沿用旧提交，最终证据见 Release 正文。

Stage2 使用有界 NodeId/edge arena 解析表达式，Stage3 在同一模型上支持 module/import/struct/enum、基础函数签名和语句、braced `if` / `while`。`while` 与 `if` 共用最多 32 层显式栈及每函数 128 条聚合 statement 上限，差分固定 span、成功和失败边界。Ku Parser 尚未迁移 `&` 参数、函数类型、泛型、async、for/try 和其余表达式；Lexer 识别 token 不代表 Parser 已支持对应语法。

现有 `KuString`、array、dynamic object、Result/Error、closure ABI 和 `std.fs/json/time` 已能承载这一阶段。扫描复用一次 `chars()`、常数复杂度 `byte_len()` 及既有 `+=` / self-push 收集路径，没有新增另一套用户写法。

剩余关键工作：完整 Ku AST/parser、跨工具链 typed Diagnostic、checker、IR、backend、自编译工具链、泛型与编译器容器，以及生产规模性能验证。Rust 仍是 bootstrap compiler；async native ABI 尚未开放。分配增长、zero-live 和 source-free fixture 是有限门槛，不是整个进程 RSS、生产吞吐或长期 soak 的证明。

完整路线见仓库 `docs/self-hosting.md`。
