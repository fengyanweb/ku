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
- struct / enum 会进入 layout table，enum variant 有稳定 tag 和 payload 字段顺序。
- array literal/index/assignment 保留元素类型，native C 从 IR 生成带长度的 array ABI。
- enum 构造、tag、payload 访问和 match 已降低为显式 CFG 与 intrinsic，不再使用 unsupported 占位。
- native C 后端已经能读取 `Result<int|bool|str, str>` ABI，生成 `{ ok, value, error }` 结构体、`ResultBranch` 分支、`BindOk` 取值和 `PropagateErr` 返回。
- native C 已支持非递归 struct、带长度 array、enum tag/payload 和嵌套 match CFG。
- LLVM 文本后端已支持非递归 struct 和 `Result<int|bool|str|struct>`。
- try/catch 的完整 native error slot、return 穿过 finally 的 IR 延迟返回、闭包 native ABI 和 async native ABI 仍是待完成边界。
- 暂不做 SSA、寄存器分配和完整 native ABI lowering。

## Result ABI 草案

当前 native C 子集固定三种基础 Result 结构：

```c
typedef struct { bool ok; int64_t value; const char* error; } KuResultInt;
typedef struct { bool ok; bool value; const char* error; } KuResultBool;
typedef struct { bool ok; const char* value; const char* error; } KuResultStr;
```

`ok(value)` 生成 `{ true, value, 0 }`，`err(message)` 和 `fail message` 生成 `{ false, zero, message }`。`?` 会变成 `if (result.ok) goto ok_block; else goto err_block;`，错误路径在 Result 返回函数中直接 `return result`。

native C 的基础 Result ABI 仍只覆盖 int/bool/str；LLVM 文本后端额外支持非递归 struct Result。对象、数组 Result、enum Result、闭包和泛型 Result 仍不支持。

## 后续 native 前置任务

1. 固定 array/struct/enum 的释放、复制、移动和嵌套所有权 ABI，消除当前 native array 只分配不释放的 prototype 边界。
2. 给 try/catch native lowering 增加显式 Error slot 或 block parameter，并补 return-through-finally 的延迟返回 block。
3. 固定闭包 ABI，包括捕获值、引用捕获和异步边界所有权。
4. LLVM 只按真实编译需求继续扩展 array/enum，不追求和解释器一次性等宽。
5. async native lowering继续拒绝，直到 task ABI、调度器嵌入方式和取消语义单独决策。
