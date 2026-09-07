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
- `try/catch/finally` 已有 `BeginTry` / `EndTry` / `BindError` 标记；可恢复错误、普通完成和 return 使用独立 finally block，return value 先写入隐藏槽，再经过 finally 返回。return 选择最近具有 finally 的 handler 及其对应返回值槽；内层仅有 catch 不能屏蔽外层 finally。错误传播仍选择最近的错误 handler，不共用返回路径的筛选规则。
- 同步 return-finally 使用每个 pending return 独立的隐藏原因槽：普通 return 为 false，已有 safepoint 选中的 timeout 为 true。只在跨出该清理尝试的 handler 边界时屏蔽新的 fail、`?` 或 return，丢弃新 Owned payload 并接回原 finish；清理内部局部 try/catch 仍按普通规则执行。void return 调用先执行副作用和既有 post-call 检查。保留三份 finally body，不增加第四份；嵌套清理不重置原绝对 deadline。同步整数算术另由下文 SyncGuard 接入；Panic/index/OOM/底层 helper 直接退出、传播所有 fatal 原因或 Task 用户 finally 仍不在已完成边界内。
- struct / enum 会进入 layout table，enum variant 有稳定 tag 和 payload 字段顺序。
- array literal/index/assignment 保留元素类型，native C 从 IR 生成带长度的 array ABI。
- enum 构造、tag、payload 访问和 match 已降低为显式 CFG 与 intrinsic，不再使用 unsupported 占位。
- native C 后端使用统一 `KuError` 和按 payload 生成的 Result ABI，生成 `ResultBranch`、消费式 `BindOk` 和只传播 Error 的 `PropagateErr`。
- native C 已支持非递归 struct、带长度 array、enum tag/payload 和嵌套 match CFG。
- native C 已支持 array/named/Result 的 move、clone、drop；解构赋值先物化全部 RHS，避免 owned swap 丢值。
- LLVM 文本后端已支持非递归 struct 和 `Result<int|bool|str|struct>`。
- 闭包/function value native ABI 已具备 typed invoke pointer、局部 RC env 和按需共享 cell。参数路径直接覆盖 Copy、`str`、array、函数值、struct、enum 与 Result，普通局部路径另覆盖 object 与 KuValue；catch/match binding、local-function self、`for` 迭代变量、Task 捕获和 async 函数值仍是明确拒绝的边界。有限源码 Task 不通过 closure ABI 实现。dynamic object/KuValue 参数路径尚无可发布的显式用户类型合同。
- 暂不做 SSA、寄存器分配和完整 native ABI lowering。

## 同步 IR 构建数量预算（0.0.18 开发中）

同步 lowering 在构建期间接纳块和指令，而不是等 finally 全部展开后才检查大小。
每函数最多10,000个块（包含入口和最后一个块），每个块最多10,000条指令；全程序
共享262,144个 construction work token。块、指令、statement/expression 访问和若干
参数批次计费，同一 AST 因 finally 或推断重复下降会重复计费。无返回注解函数的
推断 probe 与正式 lowering、提升闭包均共享额度；丢弃 probe 不退款，首次资源错误
立即返回，不被旧的推断 fallback 吞掉。

这是实验期新增的数量限制：原实现只在结束时检查最后一块指令数，并可能放入
第10,001个块；现在早期大块及累计大量小函数也会被拒绝。正常错误推断 fallback
不变，遇到资源上限时错误优先顺序可能收紧。无新用户配置或写法。

此预算不是 CPU 指令计数、字节/RSS 或 OOM 恢复合同。literal/type 的复制字节、
capture/layout 等只读扫描仍是单独的未完成边界；不能宣称
所有 clone/collect 都已在分配前获得额度。定向边界和自举回归已通过，完整集合和
精确新提交三系统 CI 仍按 [工作日志](v0.0.18-worklog.md) 单独验收。

模板插值现逐片先计费、收集/解析、下降成临时值，再沿用普通字符串 Add 的求值顺序
与所有权路径。不再预先收集全部片段、构造插值数量深度的左深临时 AST；插值自身
仍经原 parser 深度检查。这里只消除该合成深链，运行时仍是逐次 concat/clone，
可能产生二次复制成本，不是线性 string builder 或峰值内存保证。

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

这是 Ku 内部生成 C 的合同，不新增允许外部 C 长期保存 borrowed pointer 的公开 FFI。模式改变了相关生成函数的 prototype，旧 C/FFI 产物必须重新编译。LLVM 文本后端明确拒绝借用参数，使用 C backend；首片 native async 子集不支持 borrowed 参数或同步借用调用。

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
4. native C 已接通单 worker 有限源码 Task 子集，其余 async native lowering 继续拒绝。取消语义已确定，见 [语义合同](semantics.md)；执行证据见 [阶段工作日志](v0.0.18-worklog.md)。源码及 CLI 定向运行已通过，本轮表达式/清理定向测试与 Rust quality 通过；本轮 native 全集和质量检查通过，workspace 的文档失败与修复证据单列；精确新提交三系统 CI/sanitizer 仍待核实，历史失败和修复结果分别见工作日志。不能把这个子集或内部 frame 夹具通过当作完整 native async、M:N 或生产性能验收完成。

## Typed Task IR 与有限源码接入（v0.0.18 开发中）

`src/ir/task.rs` 是与同步 `IrProgram` 分离的编译器内部中间层，不增加 Ku 写法、
标准库入口或 CLI 开关。`src/ir/task_lower.rs` 从已展开并检查的 AST 生成该 IR，
仅由 native C 的显式 async main 子集选用；同步 IR、LLVM 不通过清除 async 标志回退。

当前 frame IR 使用密集 `SlotId` / `StateId`，支持 `int`、`bool`、`null`、`str`
及对应单层 Result，以及 move-only `Task { result }` 槽。操作显式区分 Init、Copy、Move、
WrapOk、Unary、Binary、Read、Drop、DropIfInit、Start、Print；控制边包括 Jump、Branch、Suspend
（resume / cleanup）、Await、TryResult、Complete 和 Terminate。
暂不支持 array/object/struct/enum、函数值、Task 参数/返回或借用参数进入 frame。

`verify_and_plan` 先验证形状、类型、资源硬限，再计算跨分支和循环的 must/may
初始化固定点及包括 cleanup/drop 用途的 liveness。普通 Value-only frame 保留原有按需
存储；hosted frame 的所有 Task 槽和 Owned Value 槽固定存储，已死亡的 Copy 临时可留
在 resume 栈上。Await ready 消费隐藏 owner 并初始化结果，cleanup edge 在 host
完成全部 Task 移交后清 Task 位；TryResult 的成功/错误边分别初始化不同槽。已死亡的普通 Owned Value 必须在挂起前显式
drop，不能为了缩 frame 擅自提前释放资源。借用值不能跨 Suspend；owned 值不能隐式
Copy、覆盖可能仍初始化的槽或再次消费 moved-from 值。Task 不能普通 Drop/DropIfInit，
Complete 留存的 Task 只能由生成的 scope drain 处理。取消区域不能回正常区域、
Complete、Start、Await 或 Suspend；本片也拒绝 cleanup 中可能溢出的 Negate 和算术
Binary，避免算术失败覆盖原取消/超时原因；总是有限且不失败的 Not/比较仍可用于内部
cleanup IR。拒绝所有不经过实际 suspension 的环，
包括 cleanup 中的环。它不是完整语言的 finally/异常或任意 Await 组合 verifier。

内部硬限为 64 函数、每函数 64 槽 / 256 状态、全程序 4096 操作、1,000,000 字面量
字节（含 UTF-8、Error 三字段和函数名）及 1,000,000 分析工作量；测试只能收紧限制。
这些是已构造 IR 的分析预算，不是整个编译器 RSS 或运行时总内存预算。
表达式新增的一/二元输入读取也计入分析工作量，没有放宽任何上限。Parser 的解析
递归上限仍为32；Task lower 对已构造 AST 使用独立的 `depth > 64` 拒绝。raw AST
预算测试不代表源码可以越过 parser/checker 的更早边界。

R3 前置操作 `WrapOk` 允许把已初始化的 primitive 局部构造为匹配的 Result，
不再只支持 `Ok` 常量。Copy primitive 保留来源；owned str 移动并清空来源。
借用 owned、类型不匹配、嵌套 Result、未初始化来源或覆盖仍活跃的 owned 结果
在 verifier 拒绝。该操作不分配、不改变 frame ABI 布局；初始化分析和跨挂起
liveness 同步跟踪它的消费行为。源码子集的 `ok(local)` 复用该操作。

`src/backend/c_task.rs` 通过统一 C 生成器复用既有 KuString / Result 的 move/drop
helper，不嵌入 runner 或源码。内部 frame ABI v2 有独立版本、目标 C `sizeof` / alignment、
初始化位、状态、结果槽、退出 metadata 和绝对 cleanup deadline；单 frame 存储上限 16 KiB。
host context 只在当前 callback 借用，不保存 callback 栈地址跨 Pending。
ABI 不兼容、短/未对齐存储、参数 header 别名、重复初始化、重复取结果和非空输出槽
会前置拒绝；失败不消费输入。entry 的 Copy 参数不清来源，Str/Result 参数才 move。

此 ABI **只允许调用者串行、单执行者** 使用保持存活的零填充对齐存储，不得复制或
篡改 live frame；owned 深层 payload 必须唯一且互不别名。clock 是可信、单调且不重入
的内部 hook。取消只能继承调用者提供的同一个绝对 deadline，不能续期；它尚不负责
创建整棵 Task 取消树的一秒预算，预算由生成的 host 建立。hosted Task 位必须先完成
移交，才允许 terminate/destroy；中途取消使用按初始化位清理的隐藏 epilogue，不能重用
已经过期的 Await pending cleanup CFG。真正 Pending 保留其已验证的 cleanup CFG。
Pending 必须先 terminate 走 cleanup，再 destroy；
destroy 不释放 caller 的 frame 存储。完成和终止不可改写，结果只能取一次；未取结果
由 destroy 释放。这里的 destroy 不是 Ku Task handle drop，亦没有并发原子裁决、
generation/wake、wait token、父子引用或独立外部 Task runtime 的链接合同。

`native_task_frame_ir_test` 验证反例与硬边界；`native_task_frame_c_test` 使用真实目标
C 编译器，覆盖独立栈上的 Pending→Resume、Move、部分初始化、多次挂起循环、
slot 63 / 逆序参数、Ok/Err payload、取消/超期、未取结果销毁和分配台账归零。
旧 ABI 参数拒绝不等于稳定的第三方 Task FFI。源码测试区分已开放子集和继续拒绝的
语法；测试结果、平台和 sanitizer 状态以阶段工作日志与精确 SHA CI 为准。

### R2 内部控制内核（内核不自驱动）

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
引用销毁控制存储。源码 Task 的接纳、预算、父子取消和等待由下述 driver/host 协作，
不能将该 pin 本身当作完整调度。timer/netpoll/blocking 和 M:N 仍未接入。

`native_task_control_test` 使用真实 R1 frame 和事件屏障，验证终态竞争、迟到结果、
owner drop/take、执行者排他、cleanup 期限缩短、引用硬限及资源归零。测试中的
typed adapter 是夹具，不是 AST lowering；race 场景通过不等于 TSan 或压力验收完成。

### R3 内部单 worker driver

`src/backend/c_task_driver.rs` 在非空内部 Task IR 的 C artifact 中提供 driver ABI v4。
R5a 固定等待字段采用版本 2，R5b.1 清理水位采用版本 3，R5b.2 等待类型和独立期限
升为版本 4；内部 C 类型名中的 `V1` 不是旧布局兼容承诺，
初始化明确拒绝旧版本。当前 Frame ABI 2、Control ABI 1、Driver ABI 4。
普通同步输出和空 Task IR 不附带该实现。它使用一个真实 OS worker、互斥锁、条件变量
以及调用方提供的固定 slot/ring 存储；不按 Task 创建线程，也没有定时重试忙轮询。
有限源码 TaskStart/Await 已复用它，但它不是 M:N、netpoll 或事件驱动 HTTP。

接纳顺序为 reserve → 构造 control/frame → commit；内部容量最多 1024，计数和预留
字节同时限流，低层失败不消费输入槽；源码 wrapper 对已 move 的实参另执行失败清理，
不会恢复用户 moved-from 变量。只有没有活跃 control/pin、没有已登记 control
时才能 rollback；可信 builder 可先经 R2 完整销毁未发布 control，否则必须
commit(ABORT)，交给可信 adapter 清理部分初始化 frame。BUILDING 也占用
resident，shutdown 不能假装它不存在。相同 driver 重复绑定 control 会拒绝；跨 driver
仍要求可信 builder 提供唯一、尚未发布的 control，这不是开放给 Ku 用户的裸能力。

每个 resident 预留一个队列位置；重复 wake 合并，RUNNING 期间用 notified 位记账。
回调返回 Pending 必须声明 YIELD 或已有进展来源的 WAIT。没有进展来源则报告内部错误，
不靠循环 poll 掩盖。take/cancel 的通知发生在 R2 最终发布之后；owner drop 把责任转入
固定 slot，再由 worker 推进，不能把“责任已转移”当作父作用域清理已经结束。

poll、drop、dispose 均在队列锁外执行。shutdown 在锁保护 registry lease 时仅执行
无回调、无分配的有界原子取消提交，不临时申请引用而在引用满载时漏取消。
driver/execution lease 保证 frame 工作期间存活；
terminal payload、未释放 owner 和迟到 lease 仍占用 resident/bytes。最终 dispose 必须
先释放实际 task 分配，再用本地 ticket 副本归还预算；generation 检查拒绝复用后的旧通知。
预留字节包含 adapter 声明的 control/frame/owned capacities；不包含 OS 栈或整个进程 RSS，
也尚无可增长 payload 的统一预算分配器，不能据此宣称完整低资源门禁通过。

shutdown 关闭接纳，批量取消并共享 min(首次关闭时刻 + 1000 ms, 继承 deadline)，重试
只能缩短。超期返回失败并保留 worker/storage，不强杀线程或提前 free；调用方必须释放
仍持有的 owner/注册引用后继续排空。destroy 必须等 resident 归零和 worker 完成访问，
Windows 还验证线程句柄结束；POSIX 最终 join 不是可硬限时的 portable OS primitive。
单调时钟失败不是时间零：driver 保持可观察的 INTERNAL/closing 故障，以已到期的
共享预算批量取消并保留 worker 执行清理。无 runnable 时使用不读时钟的条件等待，
仍接收后续 owner/BUILDING 归还；不退出后留下无人处理的队列，也不恢复普通 continuation。
故障即使随后读钟恢复也不能被清成成功，实际排空之后才允许销毁存储。

非 terminal、非 Pending 的执行错误与普通等待不同：driver 保留其引用和额度，
将 slot 标记为内部 FAULTED（snapshot 计为 parked），不能因 YIELD、普通 wake 或
失败 take 的通知重放已执行过 move 的 continuation。真实取消/owner 移交/关闭仍可
恢复清理；若清理自身再报内部错误，不自行忙循环。错误状态保持可观察，不能假造
业务 Result 或清理 ACK。该状态不新增字段、分配或用户 API。

`native_task_driver_test` 的 adapter 仍是测试夹具；真实源码由独立 source 测试验证。
用户 finally、I/O/timer/blocking、M:N 和压力/soak 不能由这些测试替代。

### R4 生成的 typed factory/handle

`src/backend/c_task_adapter.rs` 在全部内部 frame 定义之后按需生成每函数的
`KuTaskInstance_N`、`KuTaskHandle_N` 及创建、取值、move、drop adapter；不是公开
C FFI，也不新增用户 Task API。普通同步产物和空 Task IR 不生成这些代码。

内部 try_start 先校验输入/输出区间与类型，按 inline instance 和活动 Owned 字符串
容量 checked 接纳，只分配一个包含 control/frame/payload 的块。STATIC 字符串不计
Owned 分配；现有空串 concat 的一字节分配按一字节计费。Result 只检查和计费活动
分支。成功发布前没有 runnable 泄露，普通创建失败保持 Copy 输入、恢复移动参数，
未发布 control 经 R2 完整清理后再退 BUILDING 预算。关闭与已接纳创建竞争时，
成功返回的是真实但可能已取消的 Task，不能再把参数恢复给调用者。

take 是非阻塞一次尝试，经 `KuTaskAdapterTakeRequestV1` 移动
`KuTaskAdapterOutcomeV1` 中的 typed Result 和退出 metadata，随后仍须处理 owner；
这次内部 take ABI 是不兼容变更，旧生成 C 必须重编译，不保留旧输出形状兼容。
业务 Result.err 与外层 runtime failure 分开；不能仅根据 R2 FAILED 混为一类。drop 只在
driver 成功接管责任后清空 handle，不代表子任务清理已经完成。两者复用 R3 的
发布后通知。具体 callback 身份先于 typed instance 转换校验；整个 handle、runtime
存储及已知活动字符串别名先于输出头部读取拒绝。raw caller 仍须提供独立、有效、
唯一拥有的完整存储；范围校验不是任意 C 指针安全保证，也不无锁检查运行中的 frame。

cleanup 每个 safepoint 读取 live 最短 deadline，保留取消/超时原因；adapter 单次
观测到坏钟也进入 driver 的持续故障清理。裸 Suspend 仅映射 YIELD，不伪造没有
事件源的 WAIT；真正 Await 使用登记后的 WAIT。源码 host 已接通 Start/Await 和
父子作用域清理；这里不提供 array/object/struct/enum/closure Task payload 或增长堆预算。
成功 hosted take 在 payload claim 中先把活动 Owned 容量从 child 转记到 RUNNING parent，
总 reserved bytes 不变；所有 preflight/转账失败都不 move。root 先 drop 输出再释放 owner。
成功 hosted Start 现在只接纳 child instance 的新增额度；构建期间输入 Owned 容量仍在
RUNNING parent 账内，所有可失败步骤结束后，同锁将活动容量从 parent 转给 child 并发布。
原始外部 factory 仍接纳 instance 加外部输入；两条入口共用一份构造、恢复与销毁实现。
失败创建或普通局部 drop 后可能保守留额至 instance 销毁，不能称精确活跃堆或 RSS 计费。
私有 Start 请求只允许在同一个不挂起的父 callback 内使用，不能交给外部异步 builder；
即使活动容量为零也必须验证真实父 ticket、control 身份和额度下限。新增边界/故障测试与
精确提交三系统验证状态分别记录在工作日志，未开放源码动态分配表达式。
普通 Str/Result 局部在正常返回前可先释放；
取消路径仍先移交 Task，再执行 Value cleanup。首片没有用户可观察的局部析构器。
生成代码和参数检查沿用现有函数/槽/输出上限，不表示 64 MiB C artifact 上限等于
编译器 RSS 上限。实际测试证据见 [阶段工作日志](v0.0.18-worklog.md)。

### R5a 结果就绪等待内核

driver 的固定 slot 内包含一份向外等待和一份入向 waiter；没有按等待分配堆内存、
新引用或线程。内部 arm/read/detach 只服务可信 adapter，不是用户 API。登记要求父
任务正在同一 driver 执行、子任务仍有真实 owner；每个子任务最多一个等待者，
有界遍历活动等待链拒绝 self/cycle。父子 generation 和递增 epoch 同时匹配，溢出
拒绝而不回绕；NOTIFIED 是结果锁存，不再作为活动等待边。

先检查结果，再登记并复检。TAKING 仍属 Pending，必须等 R2 最终发布 TAKEN 或恢复
AVAILABLE 后由 driver 通知。RUNNING 期间通知记入 notified，PARKED 被入队，重复
通知合并；正常无事件时 worker 保持条件等待，不靠重复 poll 查结果。read 读取父
slot 锁存，不解引用已经释放或复用的子任务；旧 token 不得注销新等待。主动 detach
移除最后进展来源时安排一次恢复，不能把父任务永久留在 PARKED。

父取消注销向外结果等待并锁存 ABORTED，但保留祖先等待该父任务的入向关系；祖先
在父任务实际发布终态后才获通知。损坏的双向关系报告 INTERNAL，不冒充成功或改动
不匹配的其他等待。已知 header 别名先于输出内容读取拒绝；raw C 调用者仍须提供
有效、完整、独立且同步访问的存储，整数范围检查不证明任意指针安全。

该层只负责结果就绪；源码 Await 与下述 cleanup ACK/drain 分层复用。
最终 dispose/预算归还与逻辑清理完成是不同事件，迟到观察 lease
不能成为父作用域清理等待的条件。具体执行证据见阶段工作日志。

### R5b.1 逻辑清理收据

内部 owner_drop_receipt 复用原 owner-drop 事务；仅在唯一 owner 确实移入固定 deferred
slot 后，同锁签发完整收据。预检拒绝非空/未对齐输出和已知 header 别名，不取消或
消费输入。收据不持引用、不分配、不提供用户接口；raw 调用者必须独立保护 driver
存储，不得从裸 ticket 伪造收据或在 driver 销毁后使用它。

worker 在实际终态 poll 返回后检查 frame 已销毁、pin 已释放、owner 已释放、没有
活动 wrapper，且 payload 不再 AVAILABLE/TAKING，才发布该 generation 的清理水位。
此时只确认逻辑清理；迟到 observer 仍使 instance 与 charged bytes 保留，最终实际
free 之后才归还预算。水位跨 dispose/slot 复用和 BUILDING rollback 保存；rollback
不推进水位。有效旧收据只读水位即可确认 ACK，无需访问已释放或新占用的 control。
ABI 标记不认证恶意 raw C；水位也不是任意旧 generation 的历史结果或时间戳日志。
ACK 校验遇到不可能状态时，该 slot 锁存 INTERNAL；有效收据明确返回错误，任务不再
重复 poll，也不得归还其 lease/预算。其他任务的有效清理仍可得到 ACK，全局时钟故障
不会被误当作所有收据均失败。故障隔离不是可恢复取消，也不允许强行 free。

该层只提供收据签发和查询；ACK park/wake、独立 scope deadline 和全部兄弟
先取消的生成 drain 在上层复用它。运行验证范围和故障
处理以阶段工作日志为准，不能把 receipt ACK 当成整个递归子树物理释放或 Task 成功。

### R5b.2 内部 ACK 等待与 final-drain 期限（开发检查点）

ACK 等待复用已有单一 slot/link/ring，snapshot 显式区分 RESULT 与 CLEANUP_ACK。
有效旧收据可在子存储释放/复用后即时得到 ACK，不读取新 control，也不占用新任务
的 waiter。未完成收据登记后，只有实际清理水位或逐 slot 清理故障才通知父任务；
单独的 terminal、owner 转交、TAKING 回调或 observer 释放不是这个通知的替代品。

一个 task generation 只支持一个内部 final-drain session。调用方事先建立绝对
预算；首次调用即使直接 ACK 也绑定该期限，后续只取 min，read/detach 不重置。
普通父任务到期只唤醒内部清理 continuation，不将其 R2 phase 改为 Cancelled。
父已取消/超时时只拆普通结果等待，保留内部 ACK 登记并继承更短的已发布预算；
PUBLISHING 不能读取尚未发布的 control deadline，后置 wrapper 补足收紧。

期限已到和清理失败分别记账：先检查已有 ACK/fault，确实观察到 Pending 才锁存
CLEANUP_TIMEOUT。没有活动等待或已有 ACK 锁存时，到期只停止 timer，不臆造子任务
完成时间。已锁存的 timeout/fault 不被迟到 ACK 覆盖，也不因下个收据重新得到一秒。
两个计时原因沿用最早期限缓存，只有到期才做有界扫描，无周期 poll。

损坏的父子 reciprocal 只给当前父任务错误锁存，不清理其他父任务的新登记。被
隔离子任务的错误直接通知父队列，不依赖再次 poll 坏任务。raw callback 得到即时
ACK/error 必须在有界 quantum 内处理，不能在耗尽预算后用 Pending/YIELD 假造进展。
该层本身不开放用户 cleanup 挂起能力；上层生成多兄弟 scope drain，复用相同 ACK
合同。精确提交的三系统与 sanitizer 验收状态见工作日志，内部等待用例不能替代源码验收。

### R5c 有限源码 Task 与生成的 scope drain

`ku build --backend c` / `ku build --native` 对显式 `async fn main(): null!` 选择独立
AST→Task IR 路径，沿用 import graph 展开和 C artifact/options，不包含 runner。
展开后所有顶层 item 必须是非泛型 async 函数；参数和返回限 primitive/单层 Result。
支持直线绑定、已知 async 调用、Move/Await、ok/?、print/println、return、字符串常量 fail，
以及静态字符串；R5e 追加下节的 Copy 表达式。重复赋值、if/循环/递归、嵌套 scope、try/catch/finally、闭包、同步调用、
Task 参数/返回/容器/clone、未绑定 Task 临时和动态堆表达式仍拒绝。完整清单见
[并发文档](concurrency.md#当前-native-c-源码子集)。`ku ir` / `--emit-ir` / LLVM 仍拒绝 async。

`KuTaskValueV1` 是 move-only 内部值；Await 的隐藏 owner 计入64槽和初始化分析。
接纳/OOM 拒绝清理已 move 的源码实参，产生静态错误的 inline failed Task，不再分配。
private READY 先保存返回 Result；所有 sibling 先移交到固定 driver 槽，再等待 receipt ACK。
一个 final-drain session 共用一次绝对 D，后续更短取消预算通过有效 receipt 收紧已移交 child，
不逐个续期。正常超期 drop 暂存 Result，返回外层 `task/shutdown_timeout`，不取消正常父；
取消胜出时 drop 私有结果并保留原取消原因。取消中的 ACK continuation 不放宽用户 cleanup 禁 Await。
正常 Await 不开启新的 scope deadline；root 通过真实条件等待取值，不递归 poll child。
外层 `RUNTIME_FAILURE` 跨 Await 传播时，即使内层 final drain 已按期收到全部 ACK，
仍传递该次原始/收紧后的 D，祖先后续清理只能取更小值，不能重新计时。普通用户
Result 不携带已经成功结束的独立 scope 预算；这不改变可恢复错误的语义。

源码与两种 CLI native 构建的定向执行已经通过；本轮表达式/清理定向测试与 Rust
quality 通过，本轮 native 全集和质量检查通过，workspace 的文档失败与修复证据单列；精确新提交三系统 CI/sanitizer 仍待核实。
历史失败及修复结果分开记录于工作日志，不从旧 SHA 的通过结果外推。
Frame ABI 2、Control ABI 1、Driver ABI 4 不等于稳定外部 C FFI。
M:N、netpoll、事件驱动 HTTP、native blocking、完整 RSS 预算、性能基准与 soak 未完成。

### R5e checked Copy 表达式

`TaskUnaryOp` 只含 Negate/Not；`TaskBinaryOp` 只含 int 算术和 int/bool 的指定比较。
输入与输出均是显式 int/bool Value 槽，verifier 拒绝 Task/Owned、混合类型、borrowed
目标、未初始化输入及目标/输入别名；来源保留，目标成功后才初始化。相同左右 Copy
输入合法。倒推 liveness 同时读取两个操作数，确保左值快照跨右侧 Await 持久化，
但不把全部 Copy 临时强制存入 frame。

逻辑 And/Or 不进入 eager Binary；source lower 复用 Branch/Jump：默认 bool 结果
支配两条路径，RHS 仅在选中边执行，再从其真正结束状态到 join。内嵌 Await、`?`
或另一逻辑式可生成自己的状态，不会把 RHS 指令提前到短路分支之前。未选中 RHS
仍被 type/budget 检查；条件创建的 Task 按真实 initialized 位参加最终 scope drain。

`src/backend/c_int.rs` 的 checked i64 helper 在同步或 Task IR 含算术时共享发射一次；
比较和 Not 不需要这些 helper。检查本身避免 signed UB，失败不写目标、无分配或
exit，不改变 frame/control/driver 布局或版本（仍为2/1/4）。Negate MIN、算术越界、
MIN/-1 的除/余都报 `integer overflow`；除/余零优先报 `division by zero`，负数
除/余向零截断。Task emitter 使用空 domain/code 和静态 message，走外层
RUNTIME_FAILURE、原有 Result/drop/child drain；不是普通 USER_RESULT 或 driver
INTERNAL，也不是通过进程退出绕过清理。取消已经胜出时保留原原因和既有绝对 D。

R5e 本身没有接入同步算术；后续 R5g 的独立实现与边界如下。不能只因 Task helper
通过就宣称所有后端已统一；不新增用户错误 code 或可恢复算术 API。

### R5g 同步整数与 SyncGuard（开发中）

同步 int 的 Negate、Add/Subtract/Multiply/Divide/Remainder 调用上述 checked helper。
typed producer 后立即接 `SyncGuard { continue_block, cleanup_block }`；直接调用、
函数值调用和 array.map 也先物化结果并检查，再允许后续实参、赋值、print、`?`
或 timeout poll。raw IR 若把这类操作嵌在未正规化表达式中或缺少紧邻 guard，C
生成器拒绝，不输出可能先读取失败结果的 C。新块和指令仍按原共享预算接纳。

`src/backend/c_sync.rs` 使用线程局部、无 Owned payload 的私有返回信号。调用者
立即 take 清空信号，帧内保留首次退出原因；健康的 finally helper 不继承旧 mailbox。
普通算术 fatal 不可 catch，跳过用户 finally，但通过统一 owner epilogue 释放结构化
资源后退出；不会伪装为用户 Result.err。已选 timeout 的清理中算术失败只中止本次
清理尝试，仍尝试外层 finally，保留原绝对 D、不续期。已求值的 fresh borrow 临时
按逆序 drop，借用来源不消费；map 中止后回收已初始化输出前缀与 env。

main 和 HTTP dispatcher 是消费信号的根：先取信号，再解释返回值。HTTP 普通算术
失败为500，已经选中的 timeout 仍为504；不能由稍后的时钟检查把500升级为504。
这不是事件驱动 HTTP、M:N 或完整 fatal 闭环。Panic/index/OOM、call-depth 的直接
退出、foreign callback 重入、Task 用户 finally 仍未覆盖。LLVM 仅保留 SyncGuard
两条 CFG 边的结构占位，不提供本片 native C 的 checked 算术与 Owned 清理保证。
定向实测与失败记录见工作日志；本片完整 workspace、精确提交三系统 CI/sanitizer
和打包门禁必须独立验收，不能以旧 SHA 的结果替代。

## IR 优化队列

Ku 要做高性能 native binary，IR 不能只做语法翻译。优化 pass 按可验证顺序推进：

当前 `optimize_program` 已接入同步 `IrProgram` 的 `ku ir`、`--emit-ir`、native C 和 LLVM 输出路径；独立 Task IR 不冒用该同步优化入口。第一阶段只做确定安全的局部优化：整数/布尔纯表达式常量折叠，`if true/false` 分支折叠为 `jump`，以及由此产生的不可达 block 删除。除零、取余零、可能改变错误时机的表达式不会被折叠。

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
