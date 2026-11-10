---
title: 数组
description: 同类型数组、索引、映射及返回新数组的集合操作。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['src/stdlib/array.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 2
slug: zh-cn/v0.0.18/docs/stdlib/array
---

数组保存同一种元素类型，类型写作 `[T]`，下标从 0 开始。需要把缺少元素作为可恢复错误处理时，使用 `try_get`。

## 导入

```ku
import array from "std.array"
```

仅使用实例方法时不需要导入模块。`map` 是实例方法，不提供 `array.map` 形式。

## 接口参考

| 函数或方法                             | 返回          | 含义                                                     |
| -------------------------------------- | ------------- | -------------------------------------------------------- |
| `array.len(values:[T])`                | `int`         | 元素数量。                                               |
| `array.is_empty(values:[T])`           | `bool`        | 是否为空。                                               |
| `array.try_get(values:[T], index:int)` | `T!`          | 独立拥有的元素值；越界返回 `array/index_out_of_bounds`。 |
| `array.push(values:[T], value:T)`      | `[T]`         | 返回末尾追加一个元素的新数组。                           |
| `array.concat(left:[T], right:[T])`    | `[T]`         | 返回合并后的新数组；仅解释器。                           |
| `array.first(values:[T])`              | `T` 或 `null` | 首元素，空数组返回 null；仅解释器。                      |
| `array.last(values:[T])`               | `T` 或 `null` | 尾元素，空数组返回 null；仅解释器。                      |
| `values.map(mapper)`                   | `[U]`         | 按顺序调用函数值，收集返回值。                           |

`push` 和 `concat` 不原地修改输入，但会生成新数组，可能复制元素和分配内存。普通 `values[index]` 越界属于运行时失败；`try_get` 返回 Result，负下标也返回错误。

数据库、TCP client 等只能转移的资源句柄放入数组后，不能调用需要复制元素的方法；整个数组仍可以转移和释放。

## 完整示例

保存为 `arrays.ku`，执行 `ku run arrays.ku`；也可使用 `ku build --backend c -o arrays arrays.ku` 编译运行。

```ku
import array from "std.array"

fn main(): null! {
    values = [1, 2, 3]
    extended = values.push(4)
    doubled = extended.map(x => x * 2)
    println(array.len(values))
    println(doubled.try_get(3)?)
    try {
        values.try_get(9)?
    } catch (error) {
        println(error.code)
    }
    return ok(null)
}
```

```text
3
8
index_out_of_bounds
```

`push` 后 `values` 仍有三个元素，`extended` 有四个。`mapper` 必须匹配元素类型，函数体遵循普通所有权和 Result 规则。

## 后端支持

此示例适用于解释器和同步 native C。v0.0.18 native C 对已支持的元素和回调类型实现了 `len`、`is_empty`、`try_get`、`push`、`map`，尚未实现 `first`、`last`、`concat`。对象或函数值数组、native Task 代码还有额外限制，解释器运行成功不代表 native 必然支持。
