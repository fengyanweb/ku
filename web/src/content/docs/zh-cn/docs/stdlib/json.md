---
title: JSON 参考
description: Ku JSON 编码、解码、KuValue 转换、错误标识和后端支持范围。
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.19.md',
    'src/ir/task_lower.rs',
    'src/backend/c_task.rs',
    'src/runtime/interpreter.rs',
  ]
sidebar:
  order: 4
---

`json.encode` / `json.decode` 是本版接口，分别复用 `stringify` / `parse`；兼容名继续保留。默认保持旧底层实现，不使用 yyjson。

## 导入与接口

```ku
import json from "std.json"
```

| 接口                         | 返回       | 含义                               |
| ---------------------------- | ---------- | ---------------------------------- |
| `json.stringify(&value:any)` | `str!`     | 只读编码，返回新的紧凑 JSON 文本   |
| `json.parse(text:str)`       | `KuValue!` | 严格解析，返回独立持有内容的动态值 |
| `json.try_parse(text:str)`   | `KuValue!` | `parse` 的兼容别名                 |
| `json.encode(&value:any)`    | `str!`     | 当前开发分支中 `stringify` 的别名  |
| `json.decode(text:str)`      | `KuValue!` | 当前开发分支中 `parse` 的别名      |

`!` 表示返回 Result。用 `?` 传播错误，或用 `try/catch` 处理；通用同步编码接口不消费或修改输入；下文 native Task 字面量子集有单独的所有权限制。

## 编码与读取

以下是完整基础示例；更广泛的开发分支回归样例见[examples/json.ku](https://github.com/fengyanweb/ku/blob/main/examples/json.ku)。

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

对象键稳定排序，预期输出为：

```text
{"count":7,"name":"Ku"}
Ku
8
```

解析返回的 `KuValue` 不能直接参与整数算术。先调用 `.as_int()?` 或 `.as_str()?`；类型不匹配会返回错误。缺少对象键也会返回错误，不自动提供默认值。

## 读取动态值

| 操作                    | 返回                    | 失败条件                                                   |
| ----------------------- | ----------------------- | ---------------------------------------------------------- |
| `value["key"]` 后接 `?` | owned 的 `KuValue` 字段 | 缺键或 receiver 类型不匹配                                 |
| `value.as_int()`        | `int!`                  | 原值不是整数时返回 `value/type_mismatch`，不把字符串转整数 |
| `value.as_str()`        | `str!`                  | 原值不是字符串时返回 `value/type_mismatch`                 |
| `value.take("key")`     | `KuValue!`              | 消费对象取出子树；可能缺键或类型不支持                     |

缺键没有隐式默认值，数值和字符串也不隐式互转。这些方法与带类型对象字段及 `get_or` 不同。

## 数据与边界

支持字符串、布尔、整数、有限浮点、null、嵌套数组、字符串键对象及可表示的 struct。函数、Task、资源句柄、Result、enum 与循环引用不能编码。

| 规则        | 当前实现                                                       |
| ----------- | -------------------------------------------------------------- |
| JSON 语法   | 严格解析；拒绝注释、尾逗号、额外尾部内容与非法 Unicode         |
| 输入 / 输出 | 各最多 1,000,000 字节                                          |
| 深度预算    | 从根深度 0 计；`depth > 32` 拒绝，33 层空容器可接受，34 层拒绝 |
| 整数        | signed 64-bit；越界报错，不自动转 float                        |
| 浮点        | 有限 binary64；不保留原始数字拼写、float 标签或负零符号        |
| Unicode     | 支持中文、emoji、转义及内嵌 NUL                                |
| 重复对象键  | 解析保留最后一个值                                             |
| 键顺序      | 编码按 UTF-8 字节序稳定排序                                    |

浮点 `1.0` 可编码为 `1`；如果整数形式超出 int64，编码补 `.0` 使解码仍按浮点处理，例如 `1e20` 输出 `100000000000000000000.0`，可再次解码。有限 binary64 不承诺保留原始数字拼写、float 标签或负零符号。

## 错误

可恢复错误的 `domain` 是 `json`，常见 `code` 为 `parse_error`、`stringify_error`、`out_of_memory`。解析消息中的 `byte N` 是从 0 开始的 UTF-8 字节位置。非法输入可以在[错误与排查](/zh-cn/docs/guide/errors/)页面找到最小处理示例。

## 后端支持

解释器、默认打包 runner 和同步 native C 支持上面的通用接口。native Task 只支持下文编码子集，不支持通用解析/解码；LLVM 不提供此库。默认 runner 的 import 仍依赖原始源码位置。

本页签名与边界来自源码和规范核对；运行验证需绑定具体版本、commit 与后端。完整行为见[JSON 语法合同](https://github.com/fengyanweb/ku/blob/main/docs/syntax.md#127-json)。

## Native Task 中的标量 JSON 对象

保存为 `task-json.ku`，使用 `ku build --backend c --profile release task-json.ku -o task-json` 编译：

```ku
import json from "std.json"

async fn main(): null! {
    text = json.encode({message: "中😀", count: 2, ready: true, missing: null})?
    println(text)
    return ok(null)
}
```

预期输出为 `{"count":2,"message":"中😀","missing":null,"ready":true}`。

native Task 的 `json.encode` / `json.stringify` 只接受一个**对象字面量**，最多 64 个不同键，值仅限 `int`、`bool`、`null`、`str`。字段表达式从左到右求值，再按确定顺序输出键。构造字面量时 owned 字符串仍按通常规则 move；后续还要使用时需显式 clone。

动态对象变量、数组、嵌套对象与浮点不在这条降低路径内。重复字面量键在编译时拒绝，与通用 JSON 解析保留最后一个重复键不同。Result 错误仍需 `?` 或 `try/catch`。此子集不替换 JSON 默认实现，也不表示 async 中支持任意 JSON 操作。
