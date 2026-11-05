---
title: '包与模块'
description: '组织源码、导入标准库与管理依赖。'
kuVersion: '0.0.19'
verifiedAt: '2026-10-29'
sources: ['docs/package.md', 'src/package.rs']
sidebar:
  order: 3
---

标准库随 Ku 分发；自己的模块和第三方包通过 import 使用。当前没有官方托管的公共包目录，仓库内的 registry 是可自行部署的参考服务。

## 创建或初始化项目

```sh
ku create hello
cd hello
ku check
ku run
```

默认 basic 模板输出 `Hello Ku`。使用 `ku create my-api -t http` 选择 HTTP 模板；`-t` 等价于 `--template`。已有空目录可执行 `ku init -t cli`，它不会覆盖已有 `ku.mod` 或 `src/main.ku`。`ku template list` 与 `ku create --list` 列出同一内置模板集：basic、cli、http、json、fs、lib。

以下布局展示手写模块；项目入口由有效 `ku.mod` 的 `root` / `main` 指定，不是目录中任意同名文件。完整选项见[CLI 参考](/zh-cn/docs/tools/cli/)。

## 项目结构

```text
hello/
  ku.mod
  src/
    main.ku
    util.ku
```

`ku.mod`:

```toml
name = "hello"
version = "0.1.0"
root = "src"
main = "main.ku"
```

`src/util.ku`:

```ku
fn Greeting(): str {
    return "Hello Ku"
}
```

`src/main.ku`:

```ku
import { Greeting } from "util"

fn main() {
    println(Greeting())
}
```

在项目目录执行 `ku check`、`ku run`，输出 `Hello Ku`。用户导出名以 ASCII 大写字母开头，小写名留在文件内部。

## 导入路径

| 写法             | 解析位置                   |
| ---------------- | -------------------------- |
| `"std.json"`     | 随 Ku 分发的标准库         |
| `"util"`         | 当前 package 的 root 目录  |
| `"./util.ku"`    | 当前文件所在目录           |
| `"@math/vector"` | 已声明的 math 依赖内的模块 |

本地开发依赖通过 `dep.name.source = "file://绝对路径"` 固定来源；跨包依赖需在 `ku.mod` 声明，不能任意越过包根目录读取源码。

## Registry 与 lock

registry 依赖需要明确的 HTTPS 地址与固定的 Ed25519 公钥。`ku package resolve` 更新求解与 lock；`--locked` 按已有 lock 解析，`--offline` 只使用本地缓存。`ku package pack` 生成包，`publish` / `yank` 会向配置的 registry 写入，需要相应凭据。

完整字段、版本范围与自托管接入见仓库[Package 参考](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/package.md)。
