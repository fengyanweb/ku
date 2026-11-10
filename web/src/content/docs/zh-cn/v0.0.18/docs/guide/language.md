---
title: 语言指南
description: 学习 Ku 的变量、函数、控制流、所有权、模块与可恢复错误。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
  ]
sidebar:
  order: 0
slug: zh-cn/v0.0.18/docs/guide/language
---

从变量、函数和错误处理开始，再了解资源的归属。详细规则见[语法参考](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md)和[语言语义](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/semantics.md)。下列为教学片段，后端差异见[支持矩阵](/zh-cn/v0.0.18/docs/support/)。

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

<span id="ownership"></span>

## 所有权与只读借用

`int`、`float`、`bool`、`null` 是 Copy 值。字符串、数组、对象、结构体、enum、Result 与函数值按 Owned 处理，赋值和普通传参默认 move；需要保留独立值时显式 `.clone()`。Task 和 move-only native 句柄不可 clone。Owned 值在拥有者作用域退出时确定性 drop；复制不是自动发生的。

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Ku 的所有权语义不依赖全局追踪式垃圾回收（tracing GC）。部分共享内部结构仍使用引用计数，例如 native 闭包环境；这不表示零运行时开销、零停顿或不会发生实现缺陷。

同步函数可以声明只读借用参数，完整示例：

```ku
fn length(&text: str): int {
    return text.len()
}

fn main() {
    text = "Ku"
    println(length(text))
    println(text)
}
```

预期输出为 `2` 和 `Ku`。

调用仍写 `length(value)`，不在实参前加 `&`。借用不能修改来源、移出其 owned 内容或逃逸调用作用域；async borrowed 参数等组合尚不支持。普通 Owned 参数仍可能移动实参，即使函数体只读；是否保留所有权由参数声明决定。只读借用不代表函数没有 I/O 或内部状态变化。

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

<span id="errors"></span>

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

普通开发者通过 `async fn` 获得一次性 Task 句柄，再 `await`；没有 `task.spawn`、`Task.new` 或 `thread.spawn` 业务入口。解释器和 native Task 子集的覆盖不同，不应从一种后端的示例推导另一种后端完整支持。参见 [Task 与异步调用](/zh-cn/v0.0.18/docs/guide/concurrency/)和[平台与后端支持](/zh-cn/v0.0.18/docs/support/)。
