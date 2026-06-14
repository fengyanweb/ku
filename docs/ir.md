# Ku IR Draft

0.0.7 开始引入 IR，0.0.10 推进到 typed temp CFG + Result ok/err CFG 草案。目标是给 native C / LLVM 后端打地基，不直接从 AST 跳到 C 或 LLVM。

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

0.0.10 的 IR 是 typed temp CFG 草案层：

- 能列出顶层函数。
- 能保留参数和返回类型。
- 非叶子表达式会生成稳定 `%tN` 临时值。
- 表达式有 `IrExpr.ty`，变量首次赋值降成 typed `let`，再次赋值降成 `store`。
- `print` 有独立 IR 指令，不再和普通表达式混在一起。
- 数组/字段赋值通过 `IrLValue` 表达。
- `if` / `while` 已有基础 block 和 `Branch` / `Jump` / `Return` terminator。
- `for` 已有 `ForEach` terminator。
- `?` 会降成 `ResultBranch`，ok 分支用 `BindOk` 取值，err 分支用 `PropagateErr` 或跳入 try handler。
- `try/catch/finally` 已有 `BeginTry` / `EndTry` / `BindError` 标记，并可作为 `?` 的错误边目标。
- struct / enum 会进入 layout table。
- 复杂 `match` lowering、Result ABI 和 try/finally 的完整 native lowering 仍是待完成边界。
- 暂不做 SSA、寄存器分配和完整 native ABI lowering。

## 后续 native 前置任务

1. 固定 Result ABI，让 `ResultBranch` 可以进入 native C / LLVM 后端。
2. 完整 lowering `match`，再扩展复杂嵌套模式检查。
3. 固定 struct / enum / array / result / string 的 native ABI。
4. 继续扩展 C 后端，覆盖 Result、struct/enum 和闭包。
5. 再评估 LLVM 后端。
