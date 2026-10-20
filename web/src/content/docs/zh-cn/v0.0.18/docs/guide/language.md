---
title: 语言指南
description: 学习 Ku 的变量、函数、控制流、所有权、模块与可恢复错误。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/syntax.md
  - docs/semantics.md
  - examples/function.ku
  - examples/control_flow.ku
  - examples/result.ku
slug: zh-cn/v0.0.18/docs/guide/language
---

本文档对应 v0.0.18 实验预发行版的冻结范围；实际公开状态与下载资产以 GitHub Release 为准。这里介绍核心写法，完整规则以仓库[语法合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md)和[语义合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md)为准。下列为教学片段，后端差异见[支持矩阵](/zh-cn/v0.0.18/docs/support/)。

## 变量与类型

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

首次赋值即声明变量；基本类型为 `int`、`float`、`bool`、`str`、`null`。整数为有符号 64 位，浮点使用 binary64。不会通过 truthy 转换把整数当条件。

## 函数与控制流

完整基础函数示例来自 `examples/function.ku`：

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

预期输出 `30`。下列控制流片段可放入 `main`：

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` 遍历 `0` 到 `4`。`if` 与 `while` 条件必须是 `bool`；多语句 body 使用花括号。分支表达式使用 `match`，没有 `switch`。

## 所有权与只读借用

`int`、`float`、`bool`、`null` 是 Copy 值。字符串、数组、对象、结构体、enum、Result 与函数值按 Owned 处理，赋值和普通传参默认 move；需要保留独立值时显式 `.clone()`。Task 不可 clone。

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

同步函数可以声明只读借用参数：

```ku
fn length(&text: str): int {
    return text.len()
}
```

调用仍写 `length(value)`，不在实参前加 `&`。借用不能修改来源、移出其 owned 内容或逃逸调用作用域；async borrowed 参数等组合尚不支持。只读借用不是任意函数纯度保证。

## 结构体、对象与模块

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

动态对象使用 `{ name: "Ku" }`。普通对象索引缺键会失败；`value["name"]?` 以 Result 处理缺键，成功值为 `KuValue`，需要具体类型时用 `.as_str()?` 或 `.as_int()?`。缺键不会自动变成 `null`。

标准库使用点分路径，例如 `import json from "std.json"`；本地模块可写 `import math from "./math"`。用户顶层导出名以 ASCII 大写字母开头，小写名字保留在文件内部。

## 可恢复错误

返回类型 `T!` 表示可能成功返回 T，也可能返回 Error。`?` 传播可恢复错误，`fail` 产生错误：

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    try {
        println(read_name()?)
    } catch (error) {
        println(error.message)
    }
}
```

Error 包含 `domain`、`code`、`message`。`panic`、Task 取消和超时不由普通 `catch` 捕获。取消时的 finally 清理有共享的绝对截止预算，不保证执行完任意用户清理逻辑。

## 并发的当前边界

普通开发者通过 `async fn` 获得一次性 Task 句柄，再 `await`；没有 `task.spawn`、`Task.new` 或 `thread.spawn` 业务入口。解释器和 native Task 子集的覆盖不同，不应从一种后端的示例推导另一种后端完整支持。参见[平台与后端支持](/zh-cn/v0.0.18/docs/support/)。
