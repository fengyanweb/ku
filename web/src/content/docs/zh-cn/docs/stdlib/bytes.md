---
title: 二进制数据
description: 拥有内容的 bytes、严格 UTF-8 转换、下标及网络二进制载荷。
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  ['src/stdlib/metadata.rs', 'src/backend/c.rs', 'tests/native_net_test.rs']
sidebar:
  order: 10
---

`std.bytes` 保存拥有内容的二进制数据，可包含 NUL 和非 UTF-8 字节。此模块仅用于生成的 native C 程序，解释器尚未实现。

## 导入与接口

```ku
import bytes from "std.bytes"
```

| 函数或方法                       | 返回            | 含义                                |
| -------------------------------- | --------------- | ----------------------------------- |
| `bytes.from_str(text:str)`       | bytes 的 Result | 复制 UTF-8 字节，生成独立二进制值。 |
| `bytes.from_array(values:[int])` | bytes 的 Result | 复制整数数组，各项必须在 `0..255`。 |
| `data.len()`                     | `int`           | 字节长度。                          |
| `data.get(index:int)`            | `int!`          | 读取一个字节，返回 `0..255`。       |
| `data.to_str()`                  | `str!`          | 严格校验 UTF-8，返回独立字符串。    |
| `data.clone()`                   | bytes           | 生成独立二进制副本。                |

bytes 类型由构造函数推断，不需要另写公开类型名。构造长度上限 64 MiB。非法整数返回 `bytes/invalid_byte`；负下标或越界返回 `bytes/index_out_of_bounds`；非法文本返回 `bytes/invalid_utf8`；可恢复分配失败返回 `bytes/out_of_memory`。不会把非法文本静默替换后当作成功。

## 完整示例

保存为 `binary.ku`，构建并运行 native 产物：

```sh
ku build --backend c -o binary binary.ku
./binary
```

```ku
import bytes from "std.bytes"

fn main(): null! {
    data = bytes.from_array([65, 0, 255])?
    copy = data.clone()
    println(data.len())
    println(copy.get(1)?)
    text = bytes.from_str("中😀")?
    println(text.len())
    println(text.to_str()?)
    try {
        data.to_str()?
    } catch (error) {
        println(error.code)
    }
    return ok(null)
}
```

```text
3
0
7
中😀
invalid_utf8
```

Windows 请使用 `.exe` 输出名并运行对应文件。`ku check binary.ku` 可检查源码；`ku run binary.ku` 尚不支持 bytes。二进制值可直接交给 [TCP client 的 `write`](/zh-cn/docs/stdlib/net/)，写入不消费原 bytes。
