---
title: 对象
description: 字符串键对象、严格读取、显式默认值与 JSON 动态值。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['src/stdlib/object.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 3
slug: zh-cn/v0.0.18/docs/stdlib/object
---

对象字面量用字符串键关联值。缺键时需要默认值，使用 `get_or`；需要明确错误，使用带 `?` 的读取。

## 导入与接口

```ku
import object from "std.object"
```

| 写法                                      | 结果               | 缺键行为                             |
| ----------------------------------------- | ------------------ | ------------------------------------ |
| `object.get_or(value, key:str, fallback)` | 已有值或默认值     | 返回 fallback。                      |
| `value.get_or(key:str, fallback)`         | 同一操作的方法形式 | 返回 fallback。                      |
| `value["key"]?`                           | 解包成功读取结果   | 通过 `?` 返回 `object/missing_key`。 |
| `value["key"]`                            | 已有值             | 运行时失败。                         |
| `value.key`                               | 字段访问           | 需要适用的已知字段，不是可选读取。   |

`get_or` 不插入键，也不修改对象。键存在且值为 `null` 时仍返回 null，不改用默认值。fallback 表达式在调用前正常求值，并非延迟执行的 `get_or_else`。fallback 是拥有型参数；仅在需要继续独立使用原 fallback 时显式 `.clone()`。

## 完整示例

保存为 `objects.ku`，执行 `ku run objects.ku`；或用 `ku build --backend c -o objects objects.ku` 编译后运行。

```ku
fn main(): null! {
    user = { name: "Ku" }
    println(user.get_or("name", "unknown"))
    println(user.get_or("age", 99))
    try {
        value = user["missing"]?
        println(value)
    } catch (error) {
        println(error.code)
    }
    return ok(null)
}
```

```text
Ku
99
missing_key
```

## JSON 对象

`json.decode(text)?` 返回 `KuValue`，不是静态类型对象。参与有类型的操作前，使用 `decoded["name"]?.as_str()?` 或 `.as_int()?` 转换。完整往返和非法输入处理见 [JSON 参考](/zh-cn/v0.0.18/docs/stdlib/json/)。

## 后端支持

此示例使用解释器和同步 native C 都支持的实例方法读取。native C 请使用 `value.get_or(...)`；模块级 `object.get_or(...)` 形式目前无法正确 lowering。仅使用方法的程序不需要具名导入。native C 不支持所有推断类型仍为 unknown 的表达式，包括对象字面量的部分普通点字段访问。固定布局可使用带类型的 struct；解释器接受的字段或方法表达式不一定可直接移植。LLVM、native Task 另有限制。
