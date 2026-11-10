---
title: 文件与目录
description: 使用 std.fs 读写 UTF-8 文件、判断路径状态和读取目录。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['src/stdlib/fs.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 5
slug: zh-cn/v0.0.18/docs/stdlib/fs
---

通过 `import fs from "std.fs"` 导入。相对路径按当前 `.ku` 源文件所在目录解析，不是按终端当前目录；绝对路径保持绝对路径。

## 写入、读取与目录列表

将下列程序保存为可写目录中的 `files.ku`，执行 `ku run files.ku`。程序会在源码旁创建或**覆盖** `message.txt`，结束后可删除该文件。目录列表也会包含源码及该目录内原有文件。

```ku
import fs from "std.fs"

fn main(): null! {
    fs.write("message.txt", "你好 Ku\n")?
    text = fs.read("message.txt")?
    println(text)
    println(fs.exists("message.txt"))

    paths = fs.read_dir(".")?
    for path in paths { println(path) }

    try {
        fs.read("missing.txt")?
    } catch (err) {
        println(err.domain)
        println(err.code)
    }
    return ok(null)
}
```

程序输出 `你好 Ku`、`true`、排序后的条目路径，再为特意读取的缺失文件输出 `fs`、`read_failed`。问候文本自带换行，加上 `println` 会多一个空行。

## 接口

`T!` 表示 Result。输入参数为只读；读取结果为独立持有的字符串，目录结果为独立持有的数组。

| 调用                               | 返回     | 行为                                                  |
| ---------------------------------- | -------- | ----------------------------------------------------- |
| `fs.read(path:str)`                | `str!`   | 读取 UTF-8 文件                                       |
| `fs.try_read(path:str)`            | `str!`   | `read` 的兼容别名                                     |
| `fs.write(path:str, text:str)`     | `null!`  | 创建或截断文件，写入 UTF-8 文本                       |
| `fs.try_write(path:str, text:str)` | `null!`  | `write` 的兼容别名                                    |
| `fs.exists(path:str)`              | bool     | 能读取文件、目录或跟随后有效链接的 metadata 时为 true |
| `fs.read_dir(path:str)`            | `[str]!` | 读取一层目录，按路径排序返回                          |

`write` 不自动创建父目录、不追加内容，也不承诺原子替换；执行中断时可能已产生部分外部写入。`read_dir` 不递归，会同时包含文件和目录。返回的是解析后目录与条目名组成的路径，不只是文件名；不表示路径规范化或限制符号链接范围。

`exists` 对缺失路径、非法或过长路径、metadata 或权限错误返回 false。false 不能唯一判断为文件不存在，true 也不保证随后一定能打开。

## 限制与错误

| 操作                 | 上限                                       |
| -------------------- | ------------------------------------------ |
| `read/try_read`      | 1,000,000 个 UTF-8 字节                    |
| `write/try_write`    | 1,000,000 个 UTF-8 文本字节                |
| `read_dir`           | 10,000 项，返回路径文本合计 1,000,000 字节 |
| 目录与存在性查询路径 | 32,768 字节，包含解析后的目录前缀          |

文件错误的 `domain` 为 `fs`，常见 `code` 为 `read_failed`、`file_too_large`、`write_failed`、`content_too_large`、`read_dir_failed`；分配或资源失败可能有对应路径的错误。非法 UTF-8 会报错，不会进行有损转换。`try_read/try_write` 不改变返回类型，也不会把失败变成默认值。

使用 `?` 或示例中的 `try/catch` 处理错误。本模块目前没有公开的二进制文件、删除、重命名、创建目录或原子写入操作；二进制缓冲见[标准库目录](/zh-cn/v0.0.18/docs/stdlib/)。

## 执行支持

六个接口支持解释器、默认打包 runner 和同步 native C。受限 native Task 路径也通过固定容量的后台池支持这些文件和目录操作，调用本身可挂起，无需额外 `await`。取消不能撤回已经执行的 OS 操作，native Task 在 `finally` 中不允许提交这类操作。LLVM 不提供完整文件模块。
