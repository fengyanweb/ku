---
title: 字符串
description: Unicode 文本长度、查找、切片、转换及后端支持。
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['src/stdlib/string.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 1
---

字符串函数用于检查 UTF-8 文本或生成新字符串。只读检查后原值仍可使用。模块形式 `string.len(text)` 与方法形式 `text.len()` 调用同一操作。

## 导入

```ku
import string from "std.string"
```

只有使用 `string` 模块名时才需要这一具名导入；仅使用实例方法不需要导入。Ku 会报告未使用的具名导入。

## 接口参考

以下是文档签名；返回类型中的 `!` 表示 Result，可用 `?` 传播错误。

| 函数                                         | 返回    | 含义                                    |
| -------------------------------------------- | ------- | --------------------------------------- |
| `string.len(text:str)`                       | `int`   | Unicode 标量数量。                      |
| `string.byte_len(text:str)`                  | `int`   | UTF-8 字节数。                          |
| `string.chars(text:str)`                     | `[str]` | 按 Unicode 标量拆成可独立使用的字符串。 |
| `string.contains(text:str, needle:str)`      | `bool`  | 精确子串查找。                          |
| `string.starts_with(text:str, prefix:str)`   | `bool`  | 精确前缀判断。                          |
| `string.ends_with(text:str, suffix:str)`     | `bool`  | 精确后缀判断。                          |
| `string.slice(text:str, start:int, end:int)` | `str!`  | 按标量下标取半开区间 `[start, end)`。   |
| `string.replace(text:str, from:str, to:str)` | `str`   | 替换所有匹配项。                        |
| `string.trim(text:str)`                      | `str`   | 删除两端空白。                          |
| `string.lower(text:str)`                     | `str`   | 返回小写文本。                          |
| `string.upper(text:str)`                     | `str`   | 返回大写文本。                          |

长度统计 Unicode 标量，不是视觉字素；组合 emoji 或带组合附加符的字符可能计为多个。切片下标不是字节偏移。负下标、`start > end` 或结束位置越界，返回 `string/slice_error`。

## 完整示例

保存为 `strings.ku`，使用 `ku run strings.ku`；或使用 `ku build --backend c -o strings strings.ku` 编译后运行。

```ku
import string from "std.string"

fn main(): null! {
    text = "中😀 Ku"
    println(string.len(text))
    println(text.byte_len())
    println(text.contains("Ku"))
    println(text.slice(0, 2)?)
    letters = text.chars()
    println(letters[1])
    println(text)
    try {
        text.slice(9, 10)?
    } catch (error) {
        println(error.code)
    }
    return ok(null)
}
```

```text
5
10
true
中😀
😀
中😀 Ku
slice_error
```

## 后端支持

此示例适用于解释器和同步 native C。v0.0.18 的 Unicode `trim`、`lower`、`upper` 仅由解释器实现，native C 会拒绝编译；表中其他接口已有 native C 实现。同步示例不表示通用 native Task 或 LLVM 支持。
