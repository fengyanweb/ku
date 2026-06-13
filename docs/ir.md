# Ku IR Draft

0.0.7 开始引入 IR，0.0.8 推进到 typed CFG 草案。目标是给 native C / LLVM 后端打地基，不直接从 AST 跳到 C 或 LLVM。

## 目标

- IR 在 `parse + check` 之后生成。
- IR 不负责解释执行，解释器仍直接执行 AST。
- native 后端以后从 IR 读取函数、控制流、调用和类型布局。
- stdlib ABI 后续按 IR 类型稳定后再固定。

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
IrExpr
IrLValue
IrTerminator
IrType
```

CLI：

```powershell
ku ir examples\function.ku
```

## 当前边界

0.0.8 的 IR 是 typed CFG 草案层：

- 能列出顶层函数。
- 能保留参数和返回类型。
- 表达式有 `IrExpr.ty`，变量首次赋值降成 typed `let`，再次赋值降成 `store`。
- 数组/字段赋值通过 `IrLValue` 表达。
- `if` / `while` 已有基础 block 和 `Branch` / `Jump` / `Return` terminator。
- `for`、`try/catch/finally`、复杂 `match` 仍保留为待 lowering 的边界。
- 暂不做 SSA、寄存器分配、内存布局、ABI lowering。

## 后续 native 前置任务

1. 给表达式生成稳定临时值。
2. 完整 lowering `for`、`try/catch/finally`、`match`。
3. 固定 struct / enum / array / result 的内存布局。
4. 固定 stdlib ABI。
5. 再接 C 或 LLVM 后端。
