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
- 闭包 native ABI 和 async native ABI 仍是待完成边界。
- 暂不做 SSA、寄存器分配和完整 native ABI lowering。

## Result ABI 草案

当前 native C Result 使用统一 Error 对象，并按 payload 类型生成结构：

```c
typedef struct KuError {
    const char* domain;
    const char* code;
    const char* message;
} KuError;

typedef struct {
    bool ok;
    int64_t value;
    KuError error;
} KuResult_int;
```

`ok(value)` 会 move payload 进入 Result。`err(message)` 和 `fail message` 构造 KuError。`?` 会变成 `if (result.ok) goto ok_block; else goto err_block;`，成功分支 take payload 并清空来源 Result；错误分支只取出 KuError，再按当前函数的 Result payload 构造 Err，因此 `[int]!` 可以安全传播到 `null!`，不会错误复制不同 C struct。

native C 当前覆盖 `Result<int|bool|str|null|array|struct|enum>`；owned payload 的 clone/drop 会递归调用对应 ABI。动态 object、closure 和泛型实例化 Result 仍不支持。LLVM 文本后端继续保持较小子集。

## 后续 native 前置任务

1. 固定闭包 ABI，包括 typed invoke pointer、捕获 binding、env 生命周期和逃逸规则。
2. 固定 owned string 和动态 object ABI，使语言层 Owned 分类与 native 资源释放完全一致。
3. LLVM 只按真实编译需求继续扩展 array/enum，不追求和解释器一次性等宽。
4. async native lowering继续拒绝，直到状态机 task ABI、调度器嵌入方式和取消语义单独决策。

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
