# Ku IR Draft

0.0.7 开始引入 IR，0.0.11 推进到 typed temp CFG、Result ok/err CFG 和第一批 native Result ABI。目标是给 native C / LLVM 后端打地基，不直接从 AST 跳到 C 或 LLVM。

## 目标

- IR 在 `parse + check` 之后生成。
- IR 不负责解释执行，解释器仍直接执行 AST。
- native 后端从 IR 读取函数、控制流、调用和类型布局。
- stdlib ABI metadata 已开始固定，后续继续补运行时 ABI。

## 当前结构

Rust 模块：

```txt
src/ir/mod.rs
```

主要结构：

```txt
IrProgram
IrFunction
IrParam
IrBlock
IrInst
TempId
IrExpr
IrLValue
IrTerminator
IrType
IrLayoutTable
```

CLI：

```powershell
ku ir examples\function.ku
```

## 当前边界

0.0.11 的 IR 是 typed temp CFG 草案层：

- 能列出顶层函数。
- 能保留参数和返回类型。
- 非叶子表达式会生成稳定 `%tN` 临时值。
- 表达式有 `IrExpr.ty`，变量首次赋值降成 typed `let`，再次赋值降成 `store`。
- `print` 有独立 IR 指令，不再和普通表达式混在一起；语义是不自动追加换行。
- 数组/字段赋值通过 `IrLValue` 表达。
- `if` / `while` 已有基础 block 和 `Branch` / `Jump` / `Return` terminator。
- `for` 已有 `ForEach` terminator。
- `?` 会降成 `ResultBranch`，ok 分支用 `BindOk` 取值，err 分支用 `PropagateErr` 或 `JumpErr` 跳入 try handler。
- `try/catch/finally` 已有 `BeginTry` / `EndTry` / `BindError` 标记；错误、普通完成和 return 使用独立 finally block，return value 先写入隐藏槽，再经过 finally 返回。
- struct / enum 会进入 layout table，enum variant 有稳定 tag 和 payload 字段顺序。
- array literal/index/assignment 保留元素类型，native C 从 IR 生成带长度的 array ABI。
- enum 构造、tag、payload 访问和 match 已降低为显式 CFG 与 intrinsic，不再使用 unsupported 占位。
- native C 后端使用统一 `KuError` 和按 payload 生成的 Result ABI，生成 `ResultBranch`、消费式 `BindOk` 和只传播 Error 的 `PropagateErr`。
- native C 已支持非递归 struct、带长度 array、enum tag/payload 和嵌套 match CFG。
- native C 已支持 array/named/Result 的 move、clone、drop；解构赋值先物化全部 RHS，避免 owned swap 丢值。
- LLVM 文本后端已支持非递归 struct 和 `Result<int|bool|str|struct>`。
- 闭包/function value native ABI 已具备 typed invoke pointer、局部 RC env 和按需共享 cell。参数路径直接覆盖 Copy、`str`、array、函数值、struct、enum 与 Result，普通局部路径另覆盖 object 与 KuValue；catch/match binding、local-function self、`for` 迭代变量和 Task/async native ABI 仍是明确拒绝的边界。dynamic object/KuValue 参数路径尚无可发布的显式用户类型合同。
- 暂不做 SSA、寄存器分配和完整 native ABI lowering。

## 同步只读借用参数（0.0.17 首版实验合同）

AST 与 `IrParam` 保存 `ParamMode::Owned` / `ParamMode::View`。`View` 只是编译器内部名称，源码写 `&name: T`。`IrType::Closure` 在参数类型之外保存等长的 `param_modes`；直接调用、typed invoke、局部函数递归和 import 展开都保留槽位模式。函数类型精确匹配模式，不生成 owned / borrowed adapter。

函数数组元素的 native 调用可以保留并使用该模式。struct 的函数类型字段目前只保证 IR 保存模式，C backend 尚不支持其字段布局；不能把字段类型存在于 IR 视为 native struct 函数字段已支持。

IR dump 以 `&text: str` 表示参数；`BorrowedParam`、`BorrowedTemp` 保留非拥有来源，字段和索引投影继续传播该来源。调用的 `Borrow(value)` 表示同步期间读取 caller 所有的值，不清零来源、不插入语义 clone，也不把 borrowed 参数列入 callee drop 集合。普通 owning 参数继续使用原有 move / drop 路径。

调用者物化临时 owned 实参，在调用返回后用内部 `__ku_drop_borrow_temp` 清理。调用结果先保存，再释放临时参数并检查 post-call timeout，因此返回 Result 或继续进入 `?` / finally 不会读取已释放参数。后续参数 `?` 失败时，只清理退出的参数求值作用域；timeout 边也在进入 finally 前清理当时的借用临时，不清整个 frame。根调用返回的 owned 临时先登记，再执行该调用的 post-call timeout 检查，内部调用和循环的 safepoint 不被关闭。循环中的临时槽不能依赖下一次赋值才释放；借用别名的重写与退出清理都跳过 owning drop。Copy 参数沿用按值 ABI，在后续实参具有副作用时先保存已求值的值。

`src/ir/borrow.rs` 的 verifier 在 lowering 完成和 C backend 入口执行，优化后的 IR 也必须经过它。检查包括参数模式数量、borrowed 来源、调用模式、函数值签名，以及禁止将 borrowed owned 值移入 owning store / aggregate / return / closure，禁止写 borrowed 根、消费 borrowed Result 和通过 owned intrinsic 取得借用所有权。owned 临时来源按定义顺序传播；Copy 投影物化为独立快照，不登记为借用别名。表达式遍历也覆盖赋值目标的字段、数组下标及 cell 表达式，不能把非法 move 藏入左值。优化保留 `Borrow` / `BorrowedParam` / `BorrowedTemp`，不能通过擦除模式绕过验证。这是借用合同验证器，不是任意 raw IR 的完整 CFG 所有权证明器。

生成 C 对透明 non-Copy 参数使用只读指针，例如：

```c
int64_t Read(const KuString* text);
int64_t Inspect(const KuStruct_User* user);
```

owned 参数仍按对应值 ABI 传递。typed closure 的 invoke prototype 和类型后缀包含模式，因此 `fn(&str): int` 与 `fn(str): int` 不会共用同一签名。借用已有 place 使用稳定地址；读取 array 等投影时允许非拥有的浅 header 临时槽；借用全新表达式时由 caller 保留真实 owner。既有只读 runtime helper 读取 header / 内容，不增加另一套存储 runtime，native 借用也不引入 GC 或环境 retain。

`json.stringify` 复用只读 writer 遍历输入；typed array 直接写入输出 buffer，不再为了 JSON 转换先装箱为拥有输入的 `KuValue` array。serializer 不消费或 drop 借用输入，输出字符串仍由其 Result 拥有；新建的临时输入仍由 caller 在调用返回后清理。这只消除输入复制/错误清理，不表示 JSON 输出无需分配。

这是 Ku 内部生成 C 的合同，不新增允许外部 C 长期保存 borrowed pointer 的公开 FFI。模式改变了相关生成函数的 prototype，旧 C/FFI 产物必须重新编译。LLVM 文本后端明确拒绝借用参数，使用 C backend；native async lowering 仍沿用原有不支持边界。

当前 checker 明确拒绝 borrowed `for`、带 owned payload binding 的 borrowed match、消费式对象解构、borrowed Result `?` 以及未迁移的 stdlib borrowed 路径；这些不能被写成 native 已支持。`borrow_native_test` 与 `borrow_allocation_test` 分别覆盖可观察行为、源码删除后运行，以及嵌套读取无分配 / 临时生命周期门槛；它们不代表全量回归、三系统或 sanitizer 已完成，实际验证状态见 [v0.0.17.md](v0.0.17.md)。

## Result ABI

当前 native C Result 使用统一 Error 对象，并按 payload 类型生成结构：

```c
typedef struct KuError {
    KuString domain;
    KuString code;
    KuString message;
} KuError;

typedef struct {
    bool ok;
    int64_t value;
    KuError error;
} KuResult_int;
```

`ok(value)` 会 move payload 进入 Result。`err(message)` 和 `fail message` 构造 KuError。`?` 会变成 `if (result.ok) goto ok_block; else goto err_block;`，成功分支 take payload 并清空来源 Result；错误分支只取出 KuError，再按当前函数的 Result payload 构造 Err，因此 `[int]!` 可以安全传播到 `null!`，不会错误复制不同 C struct。

native C 当前覆盖 `Result<int|bool|str|null|array|object|struct|enum>` 的已实现组合；owned payload 的 clone/drop 会递归调用对应 ABI。并非任意动态 object/closure 组合或泛型实例化 Result 都已支持。LLVM 文本后端继续保持较小子集。

## 后续 native 前置任务

1. 逐项补齐闭包尚未支持的 binding/payload 捕获，并为每一种 owned payload 固定逃逸与失败清理测试。
2. 继续收窄动态 object 与 Result 的组合边界，不把单项 ABI 存在等同于任意嵌套组合已完成。
3. LLVM 只按真实编译需求继续扩展 array/enum，不追求和解释器一次性等宽。
4. async native lowering 继续拒绝。取消语义已确定，见 [语义合同](semantics.md)；解释器生命周期验证见 [阶段工作日志](v0.0.18-worklog.md)。实验性 frame IR、串行 frame ABI 及控制内核见下节；源码 async lowering、完整 Task 生命周期和调度器仍未实现。只有相应子集通过 IR verifier 与 native 执行测试后才能开放，不能把解释器或内部 frame 夹具通过当作源码 native async 完成。

## 实验性 task frame IR（v0.0.18 R1，未开放源码 async）

`src/ir/task.rs` 是与同步 `IrProgram` 分离的编译器内部中间层，不增加 Ku 写法、
标准库入口或 CLI 开关。它还没有从 Ku AST 生成 frame 的 lowering，也没有 Task
创建、await、I/O 或 scheduler 操作，不能用内部 Rust API 代替用户 async 的验收。

当前 frame IR 使用密集 `SlotId` / `StateId`，支持 `int`、`bool`、`null`、`str`
及对应单层 Result。操作显式区分 Init、Copy、Move、Read、Drop、DropIfInit；
控制边区分 Jump、Branch、Suspend（resume / cleanup）、Complete 和 Terminate。
暂不支持 array/object/struct/enum、函数值、子 Task 或借用参数进入 frame。

`verify_and_plan` 先验证形状、类型、资源硬限，再计算跨分支和循环的 must/may
初始化固定点及包括 cleanup/drop 用途的 liveness。只把入口参数和真正跨挂起存活
的槽放进 frame；已死亡的 Copy 临时留在 resume 栈上，dead owned 必须在挂起前显式
drop，不能为了缩 frame 擅自提前释放资源。借用值不能跨 Suspend；owned 值不能隐式
Copy、覆盖可能仍初始化的槽或再次消费 moved-from 值。取消区域不能回正常区域、
Complete 或 Suspend；本片尚无预算轮询 IR，所以拒绝所有不经过 Suspend 的环，
包括 cleanup 中的环。它不是完整语言的 finally/异常/await verifier。

内部硬限为 64 函数、每函数 64 槽 / 256 状态、全程序 4096 操作、1,000,000 字面量
字节（含 UTF-8、Error 三字段和函数名）及 1,000,000 分析工作量；测试只能收紧限制。
这些是已构造 IR 的分析预算，不是整个编译器 RSS 或运行时总内存预算。

`src/backend/c_task.rs` 通过统一 C 生成器复用既有 KuString / Result 的 move/drop
helper，不嵌入 runner 或源码。内部 frame ABI v1 有独立版本、目标 C `sizeof` / alignment、
初始化位、状态、结果槽、取消原因和绝对 cleanup deadline；单 frame 存储上限 16 KiB。
ABI 不兼容、短/未对齐存储、参数 header 别名、重复初始化、重复取结果和非空输出槽
会前置拒绝；失败不消费输入。entry 的 Copy 参数不清来源，Str/Result 参数才 move。

此 ABI **只允许调用者串行、单执行者** 使用保持存活的零填充对齐存储，不得复制或
篡改 live frame；owned 深层 payload 必须唯一且互不别名。clock 是可信、单调且不重入
的内部 hook。取消只能继承调用者提供的同一个绝对 deadline，不能续期；它尚不负责
创建整棵 Task 取消树的一秒预算。Pending 必须先 terminate 走 cleanup，再 destroy；
destroy 不释放 caller 的 frame 存储。完成和终止不可改写，结果只能取一次；未取结果
由 destroy 释放。这里的 destroy 不是 Ku Task handle drop，亦没有并发原子裁决、
generation/wake、wait token、父子引用或独立外部 Task runtime 的链接合同。

`native_task_frame_ir_test` 验证反例与硬边界；`native_task_frame_c_test` 使用真实目标
C 编译器，覆盖独立栈上的 Pending→Resume、Move、部分初始化、多次挂起循环、
slot 63 / 逆序参数、Ok/Err payload、取消/超期、未取结果销毁和分配台账归零。
旧 ABI 参数拒绝不等于已经验证不存在的外部旧 Task runtime。现有 native async
拒绝测试保持不变；测试结果、平台和 sanitizer 状态以阶段工作日志与精确 SHA CI 为准。

### R2 内部控制内核（尚无 scheduler / 源码接入）

`src/backend/c_task_control.rs` 为上述 frame 提供独立的控制 ABI v1，不改变 frame 的
串行合同。它复用已有 C 原子表示，使用 acquire/release 发布和单次 strong CAS；
竞争时返回 Pending，不用自旋等待。只有取得 executor 的执行者能调用 frame
resume/cleanup/drop；取消线程只提交控制状态，不访问正在运行的 frame。

完成/Error/panic 标签与取消/超时共享唯一裁决点。frame READY 只是私有计算完成，
不是 Task 已完成：若取消先赢，已构造结果被 drop，不能被 await 看到。取消先预约
原因，发布绝对 deadline 后才能进入清理；原因不变，后续 deadline 只能取更短值。
清理执行者在 safepoint 读取该原子期限，清理确认且 frame 销毁后才发布取消终态。
这里的 panic 只是内核终态及 owned payload 合同，不代表源码 panic 展开已接入。

owner 和内部 lease 分开；每次并发调用必须持有独立有效 lease，禁止从未保护的裸
指针 retain、复制/伪造 token 或并发修改同一个 token。引用硬限 65,536；超过或
竞争时前置拒绝，不退出进程。未完成 owner drop 请求取消；已完成但未消费的
payload 立即与 take 争取唯一所有权并释放，不随内部观察引用滞留。take 正在进行
时，drop 返回 Pending 且保留 owner；成功后 token 清空，不能再次消费。

初始化包含 owner 引用和内部生命周期 pin。**runtime 必须在暴露 owner 前接纳并
持有 driver lease，安排取消后的唤醒和有界重试**；本内核没有队列、唤醒或注册表，
不会自行排空任务。pin 仅在清理/结果提交、frame 销毁后由执行者释放；最后一个
引用销毁控制存储。源 Task 数量/字节接纳、根一秒预算创建、父子取消、wait generation、
TaskStart/await、timer/netpoll/blocking 和 M:N 仍未接入，不能将该 pin 当作完整调度。

`native_task_control_test` 使用真实 R1 frame 和事件屏障，验证终态竞争、迟到结果、
owner drop/take、执行者排他、cleanup 期限缩短、引用硬限及资源归零。测试中的
typed adapter 是夹具，不是 AST lowering；race 场景通过不等于 TSan 或压力验收完成。

## IR 优化队列

Ku 要做高性能 native binary，IR 不能只做语法翻译。优化 pass 按可验证顺序推进：

当前 `optimize_program` 已接入 `ku ir`、`--emit-ir`、native C 和 LLVM 输出路径。第一阶段只做确定安全的局部优化：整数/布尔纯表达式常量折叠，`if true/false` 分支折叠为 `jump`，以及由此产生的不可达 block 删除。除零、取余零、可能改变错误时机的表达式不会被折叠。

后续优化继续按队列推进：

1. 常量折叠：继续覆盖字符串长度、简单比较和更多不会触发错误的纯表达式。
2. 死代码删除：继续删除 `return/fail/panic/break/continue` 后不可达 block 和未使用临时。
3. 简单函数内联：只内联无递归、无捕获、体积小、无复杂错误边的函数。
4. 临时变量消除：合并单次使用的 temp，避免 C/LLVM 输出无意义中间值。
5. drop 消除：证明值未初始化、已 move、或 Copy 类型时删除 drop。
6. clone 消除：Copy clone 直接删除；源值随后不再使用时 clone-to-move。
7. escape analysis：识别不逃逸对象/数组/闭包 env，为 stack allocation 做准备。
8. stack allocation：不逃逸、定长或生命周期清楚的 native value 放栈上。
9. monomorphization 泛型特化：对实际调用的泛型实例生成具体 IR，避免动态分派。
10. bounds check 优化：循环范围和数组长度可证明时消除重复检查，但保留所有不能证明的运行时检查。
