# Ku 自举状态

结论：Ku 0.0.16 已开始自举第一阶段，但还不能自举完整工具链。

这里的“完整自举”指用 Ku 编写 Ku 编译器和工具链的主要部分，并让 Ku 编译器编译出下一代 Ku 工具链。当前 Rust 版编译器仍是 bootstrap compiler；`bootstrap/stage1` 已经是 Ku 编写的完整有界 lexer，`bootstrap/stage2` 已实现有界表达式 parser，`bootstrap/stage3` 又增加了一个可运行的语句/函数模块切片。它们用于验证语言、native ABI 和源码无关打包是否足以承载真实编译器代码，但都还不能替代完整 Rust parser，更没有替代 checker、IR 或 backend。

## 自举第一阶段已落地

- `bootstrap/stage1/token.ku`：Ku `Token` 模型使用 9 个字段记录 kind、lexeme、整数值以及完整的起止行、列和 UTF-8 byte offset；canonical 输出可稳定比较这些字段。
- `bootstrap/stage1/lexer.ku`：不用 `lexer.scan` / `parser.parse`，由 `Scan` 在 Ku 中实现 Rust lexer 当前全部 72 种 token kind，包括 ASCII identifier/关键字、整数、float、单双引号字符串、模板字符串、BOM/空白、`//`、非嵌套 `/* ... */` 注释和全部标点；错误通过 Result 返回并参与位置差分。
- Float token 在 `lexeme` 中保留未经舍入的原始十进制拼写，`int_value` 固定为 0；将它转换为 `f64` 是后续 parser 的职责，lexer 不自行实现另一套浮点舍入算法。
- 输入上限 32768 个 Unicode 字符、token 上限 4096（含 EOF）、单个解码字符串上限 4096 个 Unicode 字符。`byte_len() > 131072` 会先行拒绝必定超出字符上限的 UTF-8 输入；这些限制界定当前 Stage 1 工具的资源边界，不是生产服务的全局内存策略。
- 扫描入口只调用一次 `source.chars()`，新增的 `byte_len()` 读取 `KuString.len`，复杂度为常数；`.len()` 仍保留计算 Unicode 字符数的语义。native 和解释器从未捕获的字符数组读取元素时只复制该元素，不深拷贝整张字符表。lexeme、解码字符串与 canonical 输出用既有 `+=` 收集，避免反复扫描或复制整个前缀。
- native `chars()` 的 ASCII 元素复用 128 字节只读静态表，包括 NUL，不逐字符分配；非 ASCII 元素仍独立持有 UTF-8 正文。`bootstrap_lexer_performance_test` 检查原字符串释放后的独立生命周期、clone/concat 语义，以及 2304/4608/9216 字节输入的分配与峰值存活字节线性增长；每组重复 32 次，每轮分配台账必须归零。耗时只作诊断，不把带计量、未优化的 C 测试当作生产吞吐基准，计量也不等于整个进程 RSS。
- token 先物化为局部值，再写 `tokens = tokens.push(token)`；已去掉会反复复制整张 token 表的 `PushToken` 包装。解释器/native 仅对未捕获普通局部的纯参数 self-push 复用几何增长容量，普通 `more = tokens.push(token)` 仍返回深拷贝的新数组。没有新增另一套 builder 或可变 push API。
- 差分门槛包含 54 组完整语法/错误案例、256 组固定种子短输入，并递归收集 `bootstrap`/`examples` 下的全部 Ku 源码；当前是 357 组输入、47 份仓库源码。新增 corpus 文件会自动扩展门槛，精确数量以测试输出为准。Rust lexer 作为 token、payload、完整 span 和诊断位置的 oracle。另有 16 项字符、byte、token、字符串和长错误输入边界测试。
- 同一差分与边界 fixture 由解释器和 native 二进制运行。native 验收检查生成 C 不含 `run_source` / `const SOURCE`；编译后搬移二进制并删除完整 `.ku` 源码目录，仍能独立运行。这套门槛已在 Windows 本机闭环；Linux/macOS 工作流仍待实际运行，不能据此声称三系统已经验证。
- `bootstrap/stage2` 使用 append-only `NodeId`/edge arena 实现表达式 parser；节点上限 4096、边上限 8192、token 上限 4096、嵌套深度 32、工作步数 16384。平坦二元/后缀链用显式栈迭代处理，AST、完整 span 和诊断与 Rust parser 做 canonical 差分。
- `bootstrap/stage3` 复用同一 arena 和 Stage 2 表达式节点，增加 `Program`、普通零参数 `Function`、基础显式类型 `VarDecl`、变量 `Assign`、`ExprStmt` 与 `Return`。它单次建立 token byte offset 到 Unicode 字符索引的映射，用有界 `string.slice` 取得语句表达式，再把 Stage 2 节点重定位到原模块 span。`slice` 可能扫描完整源码，因此另设 131072 个源码字符访问的聚合预算，避免 comment-heavy 模块把扫描次数乘到语句上限；没有把 native 专用的字符串 buffer 复用误当成解释器保证。
- Stage 3 是刻意收窄的生产语法子集：语句边界沿用生产 parser 的 token grammar，已覆盖 Ku 源码通常使用的换行/空白分隔方式；已有可选分号仍能解析，但不是新增或推荐的第二种写法。函数参数、显式返回类型、复合类型和控制流尚未进入该切片。首个切片限制为 512 个 token、每个函数 128 条语句、每个模块 64 个函数，避免在 Ku arena 尚未获得原地 builder 前把 captured array 的二次增长高常数伪装成生产能力；超限和不支持的形式稳定返回 `bootstrap.parser.stage3` 诊断，不会猜测第二套语法。
- Stage 2/3 的 Rust AST canonical 差分、空输入、Unicode byte span、深度/数量边界，以及解释器/native 源码删除后运行均有独立测试。C artifact 仍是无 C compiler 环境下的硬门槛；只有链接器实际可用时才执行 native 二进制。

## 已可复用的工具链地基

- native build 已先完成 import graph 展开，不依赖原 `.ku` 模块路径。
- native ABI 已覆盖 `KuString`、array、dynamic object、Result/Error 和 closure/function value 的已实现子集及对应 move/clone/drop。常规局部捕获已验证，`for` 迭代变量捕获仍明确拒绝；这不是全部捕获形式的完成声明。array 现增加 `capacity` 字段，兼容旧两字段 C 初始化写法，但不兼容旧布局的外部二进制 ABI，相关 C/FFI 产物必须重新编译。
- `std.fs`、`std.json`、`std.time` 已有 native 闭环，足够构建有文件输入和结构化输出的小型编译工具。
- PostgreSQL/Redis/MySQL 是 native-only 生态能力；它们不是自举前端的依赖，数据库稳定工作与编译器迁移保持解耦。

## 当前缺口

1. Ku parser：已有表达式 parser 和最小函数/语句模块切片，但函数参数、返回类型、import、struct/enum、控制流、完整类型与其余表达式仍未迁移；不能称为完整 Ku parser。Stage 3 调用 Stage 2 失败时目前只保留错误 code，并把外层诊断定位到该表达式的首 token；精确 offending-token span 要等结构化 Diagnostic 跨层传递后再闭环。
2. 编译器数据模型：Token、span、Diagnostic 以及固定上限的 NodeId/edge arena 已落地，但目前节点 payload 仍是首阶段的通用字段；完整 AST 类型模型和后续 checker 所需的语义标注尚未冻结。
3. 构建性能：当前 lexer 收集路径已消除整表/前缀的重复复制，但不等于所有 Ku 容器操作均为 O(1)。字符串 `.len()` 仍按 Unicode 字符扫描；有捕获或副作用的 self-push 保留原复制语义；尚无完整编译器管线的生产规模内存、吞吐和跨平台基准。
4. 泛型与编译器常用容器仍不完整；package/registry 的 native 联网闭环也尚未进入自举路径。
5. async native ABI 仍明确不支持，不能让自举依赖未稳定的 task/runtime 能力。

## 后续顺序

1. 保持 Rust 版编译器作为可信 bootstrap compiler。
2. 把 54 组固定语法案例、256 组固定种子案例、动态仓库 corpus 与 16 项资源边界作为持续门槛，新增 Rust token、诊断或 Ku corpus 时同步扩展 Ku lexer。
3. 在现有 Stage 2/3 差分门槛上逐块加入函数签名、完整类型、import 和控制流；每个新增节点先固定 canonical AST/diagnostic，再扩展语法。
4. 让 Ku lexer + parser 的每个新增切片同时通过解释器、native C artifact 和有编译器时的删源码/搬迁验收。
5. checker、IR 和 native backend 最后迁移；每一阶段都与上一代工具链做差分测试。

## 完整自举验收

- Ku 编写的 lexer/parser 能由当前 Ku 工具链编译成可执行文件，并与 Rust 前端跑过同一套 corpus。
- Ku 编写的 Ku 编译器能编译自身下一代版本。
- bootstrap 过程可重复，产物 hash 或行为稳定。
- Windows/Linux 至少一个平台的 release pipeline 不依赖 Rust 编译器来编译 Ku 源码。
