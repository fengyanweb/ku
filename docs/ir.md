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
- `print` 有独立 IR 指令，不再和普通表达式混在一起。
- 数组/字段赋值通过 `IrLValue` 表达。
- `if` / `while` 已有基础 block 和 `Branch` / `Jump` / `Return` terminator。
- `for` 已有 `ForEach` terminator。
- `?` 会降成 `ResultBranch`，ok 分支用 `BindOk` 取值，err 分支用 `PropagateErr` 或 `JumpErr` 跳入 try handler。
- `try/catch/finally` 已有 `BeginTry` / `EndTry` / `BindError` 标记；0.0.11 增加 finally error block，让 `?` 或 `fail` 失败后能先执行 finally，再继续传播错误。
- struct / enum 会进入 layout table。
- native C 后端已经能读取 `Result<int|bool|str, str>` ABI，生成 `{ ok, value, error }` 结构体、`ResultBranch` 分支、`BindOk` 取值和 `PropagateErr` 返回。
- 复杂 `match` lowering、try/catch 的完整 native error slot、return 穿过 finally 的 IR 延迟返回、闭包 native ABI、struct/enum native ABI 仍是待完成边界。
- 暂不做 SSA、寄存器分配和完整 native ABI lowering。

## Result ABI 草案

当前 native C 子集固定三种基础 Result 结构：

```c
typedef struct { bool ok; int64_t value; const char* error; } KuResultInt;
typedef struct { bool ok; bool value; const char* error; } KuResultBool;
typedef struct { bool ok; const char* value; const char* error; } KuResultStr;
```

`ok(value)` 生成 `{ true, value, 0 }`，`err(message)` 和 `fail message` 生成 `{ false, zero, message }`。`?` 会变成 `if (result.ok) goto ok_block; else goto err_block;`，错误路径在 Result 返回函数中直接 `return result`。

这个 ABI 是 native C / LLVM 的共同前置，不覆盖数组、对象、结构体、enum、闭包或泛型 Result。

## 后续 native 前置任务

1. 给 try/catch native lowering 增加显式 error slot 或 block parameter，并补 return-through-finally 的延迟返回 block。
2. 完整 lowering `match`，再扩展复杂嵌套模式检查。
3. 固定 struct / enum / array / string 的 native ABI。
4. 固定闭包 ABI，包括捕获值、引用捕获和异步边界所有权。
5. 再评估 LLVM 后端。
