# 自举

Ku 0.0.15 还不能自举。

当前编译器和工具链主体仍由 Rust 实现。Ku 已具备解释运行、静态检查、解释器打包型 `ku build`、prototype native C 和 prototype LLVM 文本输出，但还缺少完整 native ABI 和不依赖源文件路径的最终二进制构建能力。

关键缺口：

- 完整 import graph 打包。
- native `KuString` ABI。
- native dynamic object ABI。
- native closure ABI。
- 泛型 monomorphization。
- 标准库 native runtime。
- async native 状态机 runtime。
- IR 优化和内存模型闭环。
- 编译器源码从小工具到 lexer/parser/checker 的分阶段迁移。

完整路线见仓库 `docs/self-hosting.md`。
