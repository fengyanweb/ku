# Ku IR Draft

0.0.7 开始引入 IR，0.0.9 推进到 typed temp CFG 草案。目标是给 native C / LLVM 后端打地基，不直接从 AST 跳到 C 或 LLVM。

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

0.0.9 的 IR 是 typed temp CFG 草案层：

- 能列出顶层函数。
- 能保留参数和返回类型。
- 非叶子表达式会生成稳定 `%tN` 临时值。
- 表达式有 `IrExpr.ty`，变量首次赋值降成 typed `let`，再次赋值降成 `store`。
- `print` 有独立 IR 指令，不再和普通表达式混在一起。
- 数组/字段赋值通过 `IrLValue` 表达。
- `if` / `while` 已有基础 block 和 `Branch` / `Jump` / `Return` terminator。
- `for` 已有 `ForEach` terminator。
- `try/catch/finally` 已有 `BeginTry` / `EndTry` / `BindError` 标记。
- struct / enum 会进入 layout table。
- 复杂 `match` 和 `?` 的完整失败路径 CFG 仍是待 lowering 边界。
- 暂不做 SSA、寄存器分配和完整 native ABI lowering。

## 后续 native 前置任务

1. 把 `?` / `try` 的失败路径降成显式 ok/err block。
2. 完整 lowering `match`，再做穷尽性检查。
3. 固定 struct / enum / array / result / string 的 native ABI。
4. 扩展 C 后端到 if / while / int 函数子集。
5. 再评估 LLVM 后端。
