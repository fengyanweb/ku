# Ku 自举状态

结论：Ku 0.0.16 已开始自举第一阶段，但还不能自举完整工具链。

这里的“完整自举”指用 Ku 编写 Ku 编译器和工具链的主要部分，并让 Ku 编译器编译出下一代 Ku 工具链。当前 Rust 版编译器仍是 bootstrap compiler；`bootstrap/stage1` 已经是 Ku 编写的完整有界 lexer，`bootstrap/stage2` 已实现有界表达式 parser，`bootstrap/stage3` 又增加了一个可运行的 import/函数/语句模块切片。它们用于验证语言、native ABI 和源码无关打包是否足以承载真实编译器代码，但都还不能替代完整 Rust parser，更没有替代 checker、IR 或 backend。

## 自举第一阶段已落地

- `bootstrap/stage1/token.ku`：Ku `Token` 模型使用 9 个字段记录 kind、lexeme、整数值以及完整的起止行、列和 UTF-8 byte offset；canonical 输出可稳定比较这些字段。
- `bootstrap/stage1/lexer.ku`：不用 `lexer.scan` / `parser.parse`，由 `Scan` 在 Ku 中实现 Rust lexer 当前全部 72 种 token kind，包括 ASCII identifier/关键字、整数、float、单双引号字符串、模板字符串、BOM/空白、`//`、非嵌套 `/* ... */` 注释和全部标点；错误通过 Result 返回并参与位置差分。
- Float token 在 `lexeme` 中保留未经舍入的原始十进制拼写，`int_value` 固定为 0；将它转换为 `f64` 是后续 parser 的职责，lexer 不自行实现另一套浮点舍入算法。
- 输入上限 32768 个 Unicode 字符、token 上限 4096（含 EOF）、单个解码字符串上限 4096 个 Unicode 字符。`byte_len() > 131072` 会先行拒绝必定超出字符上限的 UTF-8 输入；这些限制界定当前 Stage 1 工具的资源边界，不是生产服务的全局内存策略。
- 扫描入口只调用一次 `source.chars()`，新增的 `byte_len()` 读取 `KuString.len`，复杂度为常数；`.len()` 仍保留计算 Unicode 字符数的语义。native 和解释器从未捕获的字符数组读取元素时只复制该元素，不深拷贝整张字符表。lexeme、解码字符串与 canonical 输出用既有 `+=` 收集，避免反复扫描或复制整个前缀。
- native `chars()` 的 ASCII 元素复用 128 字节只读静态表，包括 NUL，不逐字符分配；非 ASCII 元素仍独立持有 UTF-8 正文。`bootstrap_lexer_performance_test` 检查原字符串释放后的独立生命周期、clone/concat 语义，以及 2304/4608/9216 字节输入的分配与峰值存活字节线性增长；每组重复 32 次，每轮分配台账必须归零。耗时只作诊断，不把带计量、未优化的 C 测试当作生产吞吐基准，计量也不等于整个进程 RSS。
- token 先物化为局部值，再写 `tokens = tokens.push(token)`；已去掉会反复复制整张 token 表的 `PushToken` 包装。解释器/native 仅对未捕获普通局部的纯参数 self-push 复用几何增长容量，普通 `more = tokens.push(token)` 仍返回深拷贝的新数组。没有新增另一套 builder 或可变 push API。
- 差分门槛包含 54 组完整语法/错误案例、256 组固定种子短输入，并递归收集 `bootstrap`/`examples` 下的全部 Ku 源码；当前是 360 组输入、50 份仓库源码。新增 corpus 文件会自动扩展门槛，精确数量以测试输出为准。Rust lexer 作为 token、payload、完整 span 和诊断位置的 oracle。另有 16 项字符、byte、token、字符串和长错误输入边界测试。
- 同一差分与边界 fixture 由解释器和 native 二进制运行。native 验收检查生成 C 不含 `run_source` / `const SOURCE`；编译后搬移二进制并删除完整 `.ku` 源码目录，仍能独立运行。这套门槛已在 Windows 本机以及 Windows/Linux/macOS 的远端 native target gate 闭环。
- `bootstrap/stage2` 使用 append-only `NodeId`/edge arena 实现表达式 parser；`NodeId` 的当前 ABI 固定为正的 1-based `int`，0 永久保留为“无节点”。节点上限 4096、边上限 8192、token 上限 4096、嵌套深度 32、工作步数 16384。成功输出统一经过 `ValidateParseOutput`：root 必须是最后一个节点，edge slice 必须非负、连续且在界内，child 必须严格早于 parent；children 保持源码顺序，每棵 subtree 必须占据连续的后序区间，完整 arena 最终只能归约为一棵 tree（因此也排除 orphan、重复 parent 和共享 child），节点 span 也必须有序。后续语义 arena 才负责共享或驻留。验证使用减法式范围检查，不在不可信元数据上先做加法；tree 检查使用 append-only forest link，不做会让解释器复制整个数组的 indexed assignment。flat-local 投影把重复嵌套 owned 读取造成的 O(n²) 降为 O(nodes + edges)，额外 forest storage 为 O(nodes)；但当前 value-position owned field read 仍会产生受 4096/8192 上限约束的线性副本，尚不能宣称 zero-copy，后续需由 allocation gate 量化并配合 consuming projection 优化。平坦二元/后缀链用显式栈迭代处理，AST 与完整 span 对 Rust parser 做 canonical 差分，错误诊断另有稳定 code/message/span 边界门槛。
- Stage 2/3 的 source-positioned parser failure 统一使用 `Diagnostic { severity, domain, code, message, source, span }`；wire canonical 固定为 `severity|domain|code|source|message|span`，字符串字段统一转义，缺少 token 的内部输入错误固定使用 `1:1@0..1:1@0` point span。当前公开 parser 入口没有文件名参数，所以 `source` 固定为短标识 `<source>`，不会复制源码正文或开放未完整转义的任意终端文本。`ParseContext` 只传递白名单内的 parser domain，Stage 3 的表达式错误在 Stage 2 构造时就使用 Stage 3 domain，不解析或改写 canonical 字符串。
- 这仍是临时 transport：Ku `Error`/native `KuError` 只有 `domain/code/message` 三个字段，因此完整 Diagnostic 暂时 canonical 化后放入 `Error.message`。这不是 typed Diagnostic sidecar，也不代表 Rust checker、CLI JSON、VSCode range 或 Stage 1 lexer diagnostic 已统一；后续闭环不能依赖反向解析这段字符串。
- `bootstrap/stage3` 复用同一 arena 和 Stage 2 表达式节点，增加 `Program`、`Import`、`ImportNamespace`、`ImportName`、`ImportAlias`、普通 `Function`、`Parameter`、结构化 `TypeName` / `TypeArray` / `TypeResult` / `TypeUnion`、基础显式类型 `VarDecl`、变量 `Assign`、`ExprStmt` 与 `Return`。import 只接受生产 parser 已有的三种形式：`import "path"`、`import ns from "path"`、`import { A, B as C } from "path"`；顶层 `Program` 孩子严格保持 import/function 的源码顺序，named import 与 alias 也各自保存结构化 span。参数、显式返回类型和局部声明统一经过一条 `ReadTypeAt` 路径，支持 `int`、`float`、`bool`、`str`、`null`、点分 custom、`[T]`、每一层至多一个后缀 `T!` 和唯一的 union 写法 `A | B`。优先级与 Rust parser 一致：`!` 高于 `|`，array 内递归解析完整 union；`TypeUnion` 是单个后序节点，children 按源码顺序保存所有 arm，span 从首 arm 起点覆盖到末 arm 终点，重复 arm 不在 parser 阶段去重。类型先写入一个有界的临时 NodeId/edge plan，再按后序映射到同一个 AST arena，不使用递归 AST 值。`TypeArray` / `TypeResult` 各有一个 inner child，`VarDecl` 的孩子固定为 type、initializer，类型不再拼入节点文本。每个类型节点都保留自身完整 source span，显式栈将 array/result 结构深度限制为 32，消费的 token、union arm 收集和 edge 写入都计入模块工作预算。它对完整模块扫描一次并建立 token byte offset 到 Unicode 字符索引的映射；映射器只克隆一次全模块有界 token 表，每次类型读取仅复制当前不超过 512 个 token 的窗口，不再克隆全表。源码 move 进映射器后再 move 回来。语句表达式用有界 `string.slice` 取得已经通过词法检查的窗口；Stage 2 会重新扫描这些窗口，并通过结构化 `ParseContext` 在构建 AST 或 `Diagnostic` 前完成重定位。context 会先验证 span 顺序、窗口 EOF 边界和所有偏移加法，避免无效调用逃逸成整数溢出；Stage 3 不解析错误字符串也能保留精确 offending-token span。Stage 1 自身的结构化诊断尚未接入 `ParseContext`，因此该接口不承诺重定位独立窗口中的 lexer error。`slice` 可能扫描完整源码，所以另设 131072 个源码字符访问的聚合预算，避免 comment-heavy 模块把扫描次数乘到语句上限；没有把 native 专用的字符串 buffer 复用误当成解释器保证。
- 可导入 helper 不依赖 `ParseProgram` 维持安全前置条件：`ParseTypeWindow` 会在任何 token 索引前拒绝空窗口和超过 512 个 token 的窗口，再用至多 512 步确认只有最后一个 token 是 non-type boundary；`ReadImport` 在遍历或 prefix clone 前执行 266-token 上限；`MapTokenCharacters` 在字符数组物化和 token clone 前按 131072 bytes、32768 字符、512 token 的顺序拒绝超限输入。缺失或提前出现 boundary 都返回 canonical 化的 `bootstrap.parser.stage3/invalid_token_stream`，验证步数也进入工作量计数。
- Stage 3 是刻意收窄的生产语法子集：语句边界沿用生产 parser 的 token grammar，已覆盖 Ku 源码通常使用的换行/空白分隔方式；已有可选分号仍能解析，但不是新增或推荐的第二种写法。函数类型、泛型、async、struct/enum/module 和控制流尚未进入该切片，`string`/`nil` 也不会作为类型别名接受；连续 `T!!` 也不会成为第二种 Result 写法，嵌套 Result 通过结构本身表达，例如 `[T!]!`。切片限制为 512 个 token、每个 union 层级 64 个 arm、每模块 64 个 import、每个 named import 64 个名字、每个函数 32 个参数和 128 条语句、每个模块 64 个函数；import inspection window 另有 266 token 硬上限，可完整覆盖第 65 个全 alias 项并让 64-name 语法上限先行生效。所有循环都受 EOF、类型深度、union 宽度、token/item 数量或工作预算约束，并在每轮推进显式游标；超限和不支持的形式稳定返回 `bootstrap.parser.stage3` 诊断，不会猜测第二套语法。
- Stage 2/3 的 Rust AST canonical 差分、空输入、import/function 交错顺序、Unicode/CRLF byte span、malformed import/签名错误、深度/数量边界，以及解释器/native 源码删除后运行均有独立测试。另有不经过运行时 Rust projector 生成 expected 的 checked-in goldens，固定 Stage 2 二元/调用树、Stage 3 typed function/union 树和 parser Diagnostic 的 severity/domain/code/source/message/span；escape-sensitive golden 还覆盖字段分隔符、反斜杠、CR/LF、tab 与 Unicode，避免 Rust projector 与 Ku parser 同步漂移后自证。Stage 3 这里只生成 import AST，不负责模块解析或依赖图加载；那部分仍由 Rust bootstrap compiler 承担。C artifact 仍是无 C compiler 环境下的硬门槛；只有链接器实际可用时才执行 native 二进制。

## 已可复用的工具链地基

- native build 已先完成 import graph 展开，不依赖原 `.ku` 模块路径。
- native ABI 已覆盖 `KuString`、array、dynamic object、Result/Error 和 closure/function value 的已实现子集及对应 move/clone/drop。常规局部捕获已验证，`for` 迭代变量捕获仍明确拒绝；这不是全部捕获形式的完成声明。array 现增加 `capacity` 字段，兼容旧两字段 C 初始化写法，但不兼容旧布局的外部二进制 ABI，相关 C/FFI 产物必须重新编译。
- `std.fs`、`std.json`、`std.time` 已有 native 闭环，足够构建有文件输入和结构化输出的小型编译工具。
- PostgreSQL/Redis/MySQL 是 native-only 生态能力；它们不是自举前端的依赖，数据库稳定工作与编译器迁移保持解耦。

## 当前缺口

1. Ku parser：已有表达式 parser 和含 import、基础函数签名的最小函数/语句模块切片，但完整类型、struct/enum/module、控制流与其余表达式仍未迁移；不能称为完整 Ku parser。当前 Ku `Error` 仍只有 `domain/code/message` 三个字段，不是任意 typed payload 通道；Stage 2/3 通过 `ParseContext` 在诊断序列化前完成 span/domain 重定位，而不是反向解析 `message`。
2. 编译器数据模型：Token、span、bootstrap Diagnostic schema 以及固定上限的 NodeId/edge arena 已落地，但 Diagnostic 仍缺少跨 Rust checker、CLI、编辑器与 native sidecar 的 typed transport；节点 payload 也仍是首阶段的通用字段，完整 AST 类型模型和后续 checker 所需的语义标注尚未冻结。
3. 构建性能：当前 lexer 收集路径已消除整表/前缀的重复复制，但不等于所有 Ku 容器操作均为 O(1)。字符串 `.len()` 仍按 Unicode 字符扫描；有捕获或副作用的 self-push 保留原复制语义；尚无完整编译器管线的生产规模内存、吞吐和跨平台基准。
4. 泛型与编译器常用容器仍不完整；package/registry 的 native 联网闭环也尚未进入自举路径。
5. async native ABI 仍明确不支持，不能让自举依赖未稳定的 task/runtime 能力。

## 后续顺序

1. 保持 Rust 版编译器作为可信 bootstrap compiler。
2. 把 54 组固定语法案例、256 组固定种子案例、动态仓库 corpus 与 16 项资源边界作为持续门槛，新增 Rust token、诊断或 Ku corpus 时同步扩展 Ku lexer。
3. 在现有 Stage 2/3 import/函数签名差分门槛上逐块加入完整类型和控制流；每个新增节点先固定 canonical AST/diagnostic，再扩展语法。
4. 让 Ku lexer + parser 的每个新增切片同时通过解释器、native C artifact 和有编译器时的删源码/搬迁验收。
5. checker、IR 和 native backend 最后迁移；每一阶段都与上一代工具链做差分测试。

## 完整自举验收

- Ku 编写的 lexer/parser 能由当前 Ku 工具链编译成可执行文件，并与 Rust 前端跑过同一套 corpus。
- Ku 编写的 Ku 编译器能编译自身下一代版本。
- bootstrap 过程可重复，产物 hash 或行为稳定。
- Windows/Linux 至少一个平台的 release pipeline 不依赖 Rust 编译器来编译 Ku 源码。
