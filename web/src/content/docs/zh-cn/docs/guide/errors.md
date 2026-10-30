---
title: 错误与排查
description: 定位 Ku 安装、导入、所有权、JSON 与后端能力错误。
kuVersion: '0.0.19'
verifiedAt: '2026-10-29'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
  ]
sidebar:
  order: 2
---

编译器诊断可能随实验版本收敛。报告问题时同时提供 CLI 版本、Git commit、后端、完整命令和最小源码，避免把不同版本的行为混在一起。

<a id="result-operators"></a>

## 什么时候写 `!` 和 `?`

先看符号出现的位置：

| 写法               | 含义                                                                       | 例子                   |
| ------------------ | -------------------------------------------------------------------------- | ---------------------- |
| 类型后的 `!`       | `T!` 表示 Result：成功值是 `T`，失败值是 Error。                           | `fn read_name(): str!` |
| 表达式后的 `?`     | 成功时取出值；失败时交给所在的 `try/catch`，或从当前返回 `T!` 的函数传出。 | `name = read_name()?`  |
| 布尔表达式前的 `!` | 对 `bool` 取反，与 Result 无关。                                           | `!ready`               |

`!` 不是每个函数都需要的后缀，`?` 也不是每次调用都要写。`println(...)`、`text.len()` 和普通数组索引返回普通值，不加 `?`。`?` 不表示可选值，也不会把错误转成 `null`。

### 普通入口

只做普通计算，或者在函数内通过 `try/catch` 处理错误时，可以写 `fn main()`，省略返回类型。`main` 函数本身仍是必需的入口。

```ku
fn main() {
    ready = false
    println(!ready)
}
```

这个程序输出 `true`，不需要 `: null!` 或 `return ok(null)`。

### 将错误传出

函数要把可恢复错误交给调用者时，声明返回 `T!`；成功时用 `ok(value)` 返回：

```ku
fn read_name(): str! {
    return ok("Ku")
}

fn main(): null! {
    println(read_name()?)
    return ok(null)
}
```

这里 `read_name()` 返回 Result，`?` 取出其中的字符串。若它返回错误，后续语句不再执行，错误从 `main` 传出，CLI 报错并以非零状态退出。`null!` 表示成功时不返回业务值，失败时仍能传出 Error。

不能只删除这个例子中的 `: null!` 而保留函数外层的 `?`；若要在普通 `main` 内自行处理错误，应把相应调用放入 `try/catch`，如下面的 JSON 示例。`try` 中仍使用 `?` 检查 Result；仅把一次返回 Result 的调用放进 `try`，不会自动解包或触发 `catch`。

## 先确认运行的是哪一个 Ku

```sh
ku -v
ku -h
ku check hello.ku
```

找不到 `ku` 时，直接调用解压目录的可执行文件；Windows 使用 `ku.exe`。默认 build 缺失 Rust 或链接库时，核对[安装要求](/zh-cn/docs/install/)，保留整个归档布局，不只复制 CLI 文件。

<a id="runner-build"></a>

## Runner 构建与 E0514

`ku build hello.ku` 默认使用 Debug / Runner。E0514 表示 Rust 编译器与运行时库不兼容；0.0.19 的随包库使用 Rust 1.89.0，升级系统 Rust 不会重编这份库。保留安装包中的 `ku.exe`、`libku.rlib` 和整个 `deps` 目录，避免混用不同版本。

Windows 的 MSVC Runner 还需要 Visual Studio Build Tools 的 C++ 工具和 Windows SDK。检查实际使用的程序与工具链：

```powershell
Get-Command ku -All
where.exe ku
ku -v
rustc -vV
rustup show active-toolchain
rustup toolchain list
```

反馈问题时附安装包名、校验值、完整命令和错误输出。`ku check` 与解释运行不需要 Rust；C 原生编译的工具要求见[安装说明](/zh-cn/docs/install/)。

## 读取诊断

普通检查包含文件、行列、源码标记及可用的修复帮助。工具集成可使用：

```sh
ku check --json hello.ku
```

输出格式是 **JSON Lines**，每个诊断一行，字段包含 `level`、`code`、`message`、`file`、`line`、`column`、`endLine`、`endColumn`、`notes`、`helps`。成功时静默，不能因没有 JSON 输出就判断检查未执行。

| 诊断    | 常见原因                     | 处理方式                                                 |
| ------- | ---------------------------- | -------------------------------------------------------- |
| `E0105` | 使用了 `let`                 | 改为首次赋值声明变量                                     |
| `E0302` | `if` / `while` 条件不是 bool | 写出明确的布尔比较                                       |
| `E0601` | 标准库模块名未知             | 核对 `std.json` 这类真实模块路径                         |
| `E0605` | 缺少必须的显式 import        | 添加对应模块导入，例如 `import fs from "std.fs"`         |
| `E0901` | 读取了已经 move 的值         | 需要独立值时显式 clone，或在支持的同步调用中使用只读借用 |
| `E0913` | async 函数声明 borrowed 参数 | 改为 owned 参数；需要保留原值时由调用者 clone            |
| `E0918` | 在调用处写 `&value`          | 调用写普通实参，借用模式由函数签名声明                   |

这些是已登记诊断的摘要，不是完整错误码清单。当前唯一登记来源是 [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/error.rs)。

## 可恢复错误与编译器诊断不同

运行期 `Error` 的 `domain/code` 不同于编译器 `E` 编号。例如非法 JSON 返回 `json/parse_error`，对象缺键可返回 `object/missing_key`。在返回 `T!` 的调用后使用 `?`，或通过 `try/catch` 读取错误字段。

```ku
import json from "std.json"

fn main() {
    try {
        json.decode("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

示例故意触发 JSON 解析错误并捕获，程序正常退出。此片段以 `ku run` 为例；同一输入在支持 JSON 的后端都返回 `json/parse_error`，但 `message` 可不同：解释器消息带 `json parse failed: ` 前缀，native C 的消息可省略该前缀。程序应按 `domain/code` 处理错误，不依赖完整消息文本。解析错误的 `byte N` 为从 0 开始的 UTF-8 字节位置，中文或 emoji 前后的字节偏移不等于字符数。

## native 编译被拒绝

`ku check` 成功只表示前端检查通过。native C 和 LLVM 各自有能力边界；遇到明确的不支持诊断，先查看[支持矩阵](/zh-cn/docs/support/)，不要修改超时或以别的后端成功替代验证。

`ku build --native file.ku` 不带 `-o` 时仅写 C 文件。希望编译和链接二进制时使用 `ku build --backend c file.ku`，并确认目标 C 工具链存在。

数据库的 `execution_unknown` / `execution_completed_without_result` 表示不能安全自动重放；保留请求和业务证据，先核对真实执行状态。

更多规则见[仓库诊断说明](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/diagnostics.md)。
