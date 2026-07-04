# Ku 自举状态

结论：Ku 0.0.15 还不能自举。

这里的“自举”指用 Ku 编写 Ku 编译器和工具链的主要部分，并用 Ku 编译出新的 Ku 工具链。当前 Ku 已经能解释运行、检查、生成解释器打包型二进制、输出 prototype C 和 prototype LLVM 文本，但编译器本体仍是 Rust，native ABI 还没有覆盖编译器所需的语言和标准库能力。

## 当前已具备

- 解释器优先的完整前端：lexer、parser、checker、runtime 能跑当前语法闭环。
- `ku check --json` 可给 VS Code 使用，文本诊断仍兼容。
- `ku build` 可生成解释器打包型可执行文件。
- `ku build --native` / `--backend c` 已有 native C 原型，覆盖 `int/bool/str`、非递归 struct、带长度 array、enum tag/payload、Result、`?`、`try/catch/finally`、基础控制流和部分 owned move/clone/drop。
- `ku llvm` 已有文本 `.ll` 小子集，覆盖普通函数、基础控制流、非递归 struct 和基础/struct Result。
- package / registry / lockfile 已有严格 schema、版本范围、冲突检测、受限解包和 Ed25519 verifier 的基础能力。

## 自举缺口

1. 完整 import graph 打包：最终二进制不能依赖原 `.ku` 源码路径。
2. native `KuString` ABI：需要 UTF-8 `{ ptr, len, capacity, storage }`、owned/static 区分、move 清空源、clone 深拷贝、drop 只释放 owned。
3. native dynamic object ABI：需要 hash table、严格缺键错误、move/clone/drop 和 JSON/object 互操作。
4. native closure ABI：函数值、捕获环境、间接调用、Copy 捕获、Owned 捕获都要进入 native。
5. 泛型 monomorphization：编译器和标准库会自然需要泛型容器、Result payload 和函数抽象。
6. 标准库 native runtime：至少需要 `fs`、`json`、`time`、`package/registry` 相关能力进入 native runtime。
7. async native ABI：当前 native 明确拒绝 async/await；要等状态机 runtime、task ABI、取消/超时语义单独稳定。
8. IR 优化和内存模型闭环：常量折叠已有第一阶段，还需要死代码删除、函数内联、临时变量消除、drop/clone 消除、escape analysis、stack allocation、bounds check 优化等。
9. 编译器源码迁移策略：需要先用 Ku 写小型真实项目和工具，再迁移 lexer/parser/checker 子集，不能直接一次性重写整套编译器。

## 推荐路线

1. 保持 Rust 版编译器作为 bootstrap compiler。
2. 用 Ku 写两个真实项目验证语言和标准库，不急着迁移编译器。
3. 补齐 native `KuString`、dynamic object、closure 和完整 import graph 打包。
4. 让 Ku 先能 native 编译一个小型命令行工具，再编译一个使用包/JSON/文件的工具。
5. 再开始迁移 lexer 和 parser；checker、IR、native backend 最后迁移。

## 验收标准

Ku 可以说“开始自举”时至少要满足：

- Ku 写的 lexer/parser 能由当前 Ku 工具链编译成可执行文件。
- 生成物不读取原 `.ku` 源码依赖路径。
- native ABI 覆盖 string/object/array/Result/closure/drop/clone。
- 同一套测试能用 Rust 编译器和 Ku 编译出的工具链分别跑过。

Ku 可以说“完成自举”时至少要满足：

- Ku 编写的 Ku 编译器能编译自身下一代版本。
- bootstrap 过程可重复，产物 hash 或行为稳定。
- Windows/Linux 至少一个平台的 release pipeline 不依赖 Rust 编译器来编译 Ku 源码。
