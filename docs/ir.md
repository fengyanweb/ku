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
4. async native lowering 继续拒绝，直到状态机 task ABI、调度器嵌入方式和取消语义单独决策。

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
