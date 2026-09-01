# 自举

Ku 0.0.16 已完成第一阶段的有界 Ku Lexer，但还不能自举完整编译器。

`bootstrap/stage1/lexer.ku` 的 `Scan(source): [Token]!` 使用 Ku 实现当前全部 72 种 token，包括字符串、模板、十进制 float、注释和完整 UTF-8 byte span，不调用 Rust 的 `lexer.scan`。Float 保留原始拼写，数值转换留给后续 parser。当前上限为 32768 个 Unicode 字符、4096 个 token（含 EOF）、4096 个解码字符串字符。

349 组差分与 16 项边界用例已在 Windows 的解释器/native 路径通过。native 编译包含本地 import graph，生成的 C 不含源码 runner，搬移二进制并删除源码后仍可运行；Linux/macOS 仍待对应真机 CI 验证。

现有 `KuString`、array、dynamic object、Result/Error、closure ABI 和 `std.fs/json/time` 已能承载这一阶段。扫描复用一次 `chars()`、常数复杂度 `byte_len()` 及既有 `+=` / self-push 收集路径，没有新增另一套用户写法。

剩余关键工作：Ku AST/parser、checker、IR、backend、自编译工具链、泛型与编译器容器，以及生产规模性能和跨平台验证。Rust 仍是 bootstrap compiler；async native ABI 尚未开放。

完整路线见仓库 `docs/self-hosting.md`。
