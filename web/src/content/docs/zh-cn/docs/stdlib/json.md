---
title: JSON 参考
description: Ku JSON 编码、解码、KuValue 转换、错误标识和后端支持范围。
kuVersion: '0.0.20'
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

`std.json` 提供 JSON 编码与解码。通用同步 `json.encode` 对值进行只读编码，`json.decode` 将 JSON 文本解析为独立持有内容的动态值；两者返回 Result。`stringify`、`parse` 和 `try_parse` 作为兼容名称保留。这些接口在 0.0.20 中继续提供。当前仍使用旧 JSON 默认实现，不使用 yyjson。

## 导入与接口

```ku
import json from "std.json"
```

| 接口                         | 返回       | 含义                               |
| ---------------------------- | ---------- | ---------------------------------- |
| `json.encode(&value:any)`    | `str!`     | 只读编码，返回新的紧凑 JSON 文本   |
| `json.decode(text:str)`      | `KuValue!` | 严格解析，返回独立持有内容的动态值 |
| `json.stringify(&value:any)` | `str!`     | `encode` 的兼容名称                |
| `json.parse(text:str)`       | `KuValue!` | `decode` 的兼容名称                |
| `json.try_parse(text:str)`   | `KuValue!` | `decode` 的兼容名称                |

`!` 表示返回 Result。用 `?` 传播错误，或用 `try/catch` 处理；编码接口不消费或修改输入。下文 Task 章节说明局部 JSON 的所有权与类型限制。

## 编码与读取

以下是完整基础示例；更多情况见[examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.20/examples/json.ku)。

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.encode(value)?
    println(text)

    decoded = json.decode(text)?
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

解释器、`ku build --backend runner` 和同步 native C 支持上面的通用接口。native Task 支持局部 JSON 的解码、字符串键读取、子树移交、转换及再编码，具体范围见下文；LLVM 不提供此库。Runner 的 import 仍依赖原始源码位置。

完整行为见[JSON 语法合同](https://github.com/fengyanweb/ku/blob/v0.0.20/docs/syntax.md#127-json)。

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

这个示例使用标量字段，仍可作为最小写法。Task 中构造的动态对象字面量最多 64 个不同键，字段表达式从左到右求值；重复字面量键在编译时拒绝，与 JSON 解码保留最后一个重复键不同。构造对象时 owned 字符串和动态值字段按通常 move 规则移入对象。

## Native Task 中的局部值与跨等待

0.0.20 已支持 `json.decode()` 的局部结果跨 `await` 保存，以及字符串键索引、`take`、`as_int`、`as_str` 和 `json.encode`。下面直接使用已发布的 [examples/task_json.ku](https://github.com/fengyanweb/ku/blob/v0.0.20/examples/task_json.ku)：

```ku
import json from "std.json"
import time from "std.time"
async fn tick():int! { time.sleep(2)? return ok(1) }
async fn main():null! {
    decoded=json.decode("{\"counter\":7,\"data\":{\"message\":\"中文😀\",\"items\":[1,true,null]}}")?
    selected=decoded["counter"]?
    counter=selected.as_int()?
    copied=decoded["data"]?
    next=(await tick())?
    data=decoded.take("data")?
    body=json.encode({counter:counter+next,id:"rid",data:data,copy:copied})?
    println(body)
    return ok(null)
}
```

等待后编码输出的 `counter` 字段为 8；`data` 和 `copy` 都保留嵌套对象、中文、emoji 及解码得到的数组。Result 仍用 `?` 或 `try/catch`，不会自动解包。完整合同见 [Task JSON 指南](https://github.com/fengyanweb/ku/blob/v0.0.20/docs/native-task-json.md)。

- `json.encode(value)` 只读输入，编码后原值仍可使用。
- `value["key"]` 返回独立 owned 值；释放原根不会使选出的值失效。
- `value.take("key")` 消费原根并移交子树；之后不能继续读取原根。
- `as_int()`／`as_str()` 消费转换输入。构造输出对象时，动态值字段按 move 规则移入。
- 分配前预付 Task 字节预算，实际释放后才退款；返回、错误、取消和期限退出均负责清理。账本是 Ku 申请的堆字节，不是 RSS，也不涵盖分配器或第三方库的全部内存。

沿用原默认 JSON 引擎、严格语法及上面的深度规则；输入和输出各最多 1,000,000 字节。预算不足返回错误。单次 JSON 操作有大小限制，但不会在内部逐步挂起，不是任意指令级抢占。

解码得到的 JSON 对象、数组、字符串、数值、布尔和 null 可以保存、移交及再编码；这不表示支持动态 JSON 的整数索引或异构数组字面量。任意 JSON 的 Task 参数、公开返回值和闭包捕获仍未扩大；native Task 浮点局部变量及未列出的操作仍受原类型子集限制。不支持的程序在编译时拒绝。
