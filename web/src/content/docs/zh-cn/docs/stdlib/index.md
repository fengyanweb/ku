---
title: 标准库概览
description: 按用途查找 Ku 标准库、基础接口与不同后端的使用边界。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
  ]
---

本文档对应 v0.0.18 实验预发行版的冻结范围；实际公开状态与下载资产以 GitHub Release 为准。这里是经过源码核对的核心入口，不是全部 API 的完整参考。具体行为遵循[语法合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md)，后端范围见[支持矩阵](/zh-cn/docs/support/)。

## 导入模块

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`、`http`、`config`、`task`、`pg`、`redis`、`mysql`、`bytes`、`net` 需要显式导入。部分历史模块仍可直接调用；新示例优先显式导入，使依赖清楚。`std.task` 是内部观测与压力测试命名空间，不作为普通业务并发 API。

| 模块                                 | 用途                               | 需要注意                                               |
| ------------------------------------ | ---------------------------------- | ------------------------------------------------------ |
| `std.string` / 字符串方法            | Unicode 长度、字节长度、查找、切片 | native `trim/lower/upper` 尚无 Unicode 实现            |
| `std.array` / 数组方法               | 长度、取值、创建新数组、map        | 索引失败与可恢复 `try_get` 不同                        |
| `std.object`                         | 显式宽松读取 `get_or`              | 普通缺键索引不会默认返回 null                          |
| [std.json](/zh-cn/docs/stdlib/json/) | 严格 JSON 编码与解码               | native Task 与 LLVM 不支持                             |
| `std.fs`                             | 读写文件、存在性、目录读取         | 相对路径按当前 `.ku` 文件目录解析                      |
| `std.time`                           | 时间、日期、时长、sleep            | 同步与 Task 路径等待方式不同                           |
| `std.http`                           | HTTP 客户端、服务与响应            | 保留后端和生命周期差异；不是通用 Task 网络完成声明     |
| `std.config`                         | 环境与配置读取                     | 不应把实际凭据写进源码                                 |
| `std.pg` / `std.redis` / `std.mysql` | 统一 client 与有界连接池           | native-only；实库验收和 TLS 边界需单独核对             |
| `std.bytes` / `std.net`              | 二进制数据、TCP client 与可选 TLS  | 当前 generated native C 路径；目标 TLS pack 有额外要求 |
| `std.lexer` / `std.parser`           | 编译器调试辅助                     | 返回调试摘要，不是完整 Token / AST 公共结构            |

## 常用签名

以下是文档中的接口签名，`&` 表示已支持的只读参数模式，`T!` 表示可恢复错误结果：

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

`"中😀".len()` 为 2，`.byte_len()` 为 7；长度计 Unicode 标量值，切片下标也按字符而不是 UTF-8 字节。`array.push` 与 `array.concat` 返回新数组，不原地修改输入。

## 文件读取示例

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

在源文件同目录准备 UTF-8 的 `message.txt`。不存在或读取失败时返回错误；该示例不是不依赖输入文件的自包含程序。`try_read` / `try_write` 是保留的同类型兼容别名。

`read_dir` 返回排序后的独立路径数组，最多 10,000 项、路径文本合计 1,000,000 字节。受限 native Task 文件操作使用固定后台池；这不代表网络调用已经使用同一通用等待机制。

## 参考如何维护

当前公开接口元数据位于 [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/stdlib/metadata.rs)，实现与规范一起用于核查。本网站暂时维护人工整理的核心参考；当前没有已确认的 `ku doc` 命令。
