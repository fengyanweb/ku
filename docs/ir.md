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
