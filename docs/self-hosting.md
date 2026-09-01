# Ku 自举状态

结论：Ku 0.0.16 已开始自举第一阶段，但还不能自举完整工具链。

这里的“完整自举”指用 Ku 编写 Ku 编译器和工具链的主要部分，并让 Ku 编译器编译出下一代 Ku 工具链。当前 Rust 版编译器仍是 bootstrap compiler；`bootstrap/stage1` 已经是 Ku 编写的完整有界 lexer，用来验证语言、native ABI 和源码无关打包是否足以承载第一块真实编译器代码。Ku parser、checker、IR 和 backend 仍未自举，因此这一阶段还不能替代 Rust 编译器。

## 自举第一阶段已落地

- `bootstrap/stage1/token.ku`：Ku `Token` 模型使用 9 个字段记录 kind、lexeme、整数值以及完整的起止行、列和 UTF-8 byte offset；canonical 输出可稳定比较这些字段。
- `bootstrap/stage1/lexer.ku`：不用 `lexer.scan` / `parser.parse`，由 `Scan` 在 Ku 中实现 Rust lexer 当前全部 72 种 token kind，包括 ASCII identifier/关键字、整数、float、单双引号字符串、模板字符串、BOM/空白、`//`、非嵌套 `/* ... */` 注释和全部标点；错误通过 Result 返回并参与位置差分。
- Float token 在 `lexeme` 中保留未经舍入的原始十进制拼写，`int_value` 固定为 0；将它转换为 `f64` 是后续 parser 的职责，lexer 不自行实现另一套浮点舍入算法。
- 输入上限 32768 个 Unicode 字符、token 上限 4096（含 EOF）、单个解码字符串上限 4096 个 Unicode 字符。`byte_len() > 131072` 会先行拒绝必定超出字符上限的 UTF-8 输入；这些限制界定当前 Stage 1 工具的资源边界，不是生产服务的全局内存策略。
- 扫描入口只调用一次 `source.chars()`，新增的 `byte_len()` 读取 `KuString.len`，复杂度为常数；`.len()` 仍保留计算 Unicode 字符数的语义。native 和解释器从未捕获的字符数组读取元素时只复制该元素，不深拷贝整张字符表。lexeme、解码字符串与 canonical 输出用既有 `+=` 收集，避免反复扫描或复制整个前缀。
- native `chars()` 的 ASCII 元素复用 128 字节只读静态表，包括 NUL，不逐字符分配；非 ASCII 元素仍独立持有 UTF-8 正文。`bootstrap_lexer_performance_test` 检查原字符串释放后的独立生命周期、clone/concat 语义，以及 2304/4608/9216 字节输入的分配与峰值存活字节线性增长；每组重复 32 次，每轮分配台账必须归零。耗时只作诊断，不把带计量、未优化的 C 测试当作生产吞吐基准，计量也不等于整个进程 RSS。
- token 先物化为局部值，再写 `tokens = tokens.push(token)`；已去掉会反复复制整张 token 表的 `PushToken` 包装。解释器/native 仅对未捕获普通局部的纯参数 self-push 复用几何增长容量，普通 `more = tokens.push(token)` 仍返回深拷贝的新数组。没有新增另一套 builder 或可变 push API。
- 差分门槛包含 349 组输入：54 组完整语法/错误案例、256 组固定种子短输入，以及 `bootstrap`/`examples` 下 39 份 Ku 源码；Rust lexer 作为 token、payload、完整 span 和诊断位置的 oracle。另有 16 项字符、byte、token、字符串和长错误输入边界测试。
- 同一差分与边界 fixture 由解释器和 native 二进制运行。native 验收检查生成 C 不含 `run_source` / `const SOURCE`；编译后搬移二进制并删除完整 `.ku` 源码目录，仍能独立运行。这套门槛已在 Windows 本机闭环；Linux/macOS 工作流仍待实际运行，不能据此声称三系统已经验证。

## 已可复用的工具链地基

- native build 已先完成 import graph 展开，不依赖原 `.ku` 模块路径。
- native ABI 已覆盖 `KuString`、array、dynamic object、Result/Error 和 closure/function value 的已实现子集及对应 move/clone/drop。常规局部捕获已验证，`for` 迭代变量捕获仍明确拒绝；这不是全部捕获形式的完成声明。array 现增加 `capacity` 字段，兼容旧两字段 C 初始化写法，但不兼容旧布局的外部二进制 ABI，相关 C/FFI 产物必须重新编译。
- `std.fs`、`std.json`、`std.time` 已有 native 闭环，足够构建有文件输入和结构化输出的小型编译工具。
- PostgreSQL/Redis/MySQL 是 native-only 生态能力；它们不是自举前端的依赖，数据库稳定工作与编译器迁移保持解耦。

## 当前缺口

1. Ku parser：还没有 Ku 编写的 AST parser，更没有 checker、IR 或 C backend 的 Ku 版本。
2. 编译器数据模型：Token 和词法 span 已稳定，但仍需要 Ku AST 与 diagnostic 数据结构，而不是历史 `parser.parse` 的摘要字符串。
3. 构建性能：当前 lexer 收集路径已消除整表/前缀的重复复制，但不等于所有 Ku 容器操作均为 O(1)。字符串 `.len()` 仍按 Unicode 字符扫描；有捕获或副作用的 self-push 保留原复制语义；尚无完整编译器管线的生产规模内存、吞吐和跨平台基准。
4. 泛型与编译器常用容器仍不完整；package/registry 的 native 联网闭环也尚未进入自举路径。
5. async native ABI 仍明确不支持，不能让自举依赖未稳定的 task/runtime 能力。

## 后续顺序

1. 保持 Rust 版编译器作为可信 bootstrap compiler。
2. 把现有 349 组 lexer parity 与 16 项资源边界作为持续门槛，新增 Rust token 或诊断时同步扩展 Ku lexer。
3. 在现有完整 token/span/byte-offset 基础上开始 Ku parser 子集，并建立稳定的 AST/diagnostic 模型。
4. 让 Ku lexer + parser native 编译同一批 fixture，并继续执行删源码/搬迁验收。
5. checker、IR 和 native backend 最后迁移；每一阶段都与上一代工具链做差分测试。

## 完整自举验收

- Ku 编写的 lexer/parser 能由当前 Ku 工具链编译成可执行文件，并与 Rust 前端跑过同一套 corpus。
- Ku 编写的 Ku 编译器能编译自身下一代版本。
- bootstrap 过程可重复，产物 hash 或行为稳定。
- Windows/Linux 至少一个平台的 release pipeline 不依赖 Rust 编译器来编译 Ku 源码。
