---
title: 配置文件
description: 通过 std.config 从源码相对路径读取 .env 和平面 YAML 配置。
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['src/stdlib/config.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 7
---

通过 `import config from "std.config"` 导入。本模块读取文件；`config.env()` **不会**读取或合并进程环境变量。

## 读取配置

在 `config.ku` 旁创建以下三个文件。

`.env`：

```dotenv
APP_NAME=Ku
```

`extra.env`：

```dotenv
MODE="development"
```

`app.yaml`：

```yaml
port: 8080
debug: true
```

将程序保存为 `config.ku`，执行 `ku run config.ku`：

```ku
import config from "std.config"

fn main(): null! {
    env = config.env()
    println(env.APP_NAME)
    extra = config.env_file("extra.env")
    println(extra.MODE)

    settings = config.yaml("app.yaml")?
    println(settings.port)
    println(settings.debug)
    return ok(null)
}
```

输出：

```text
Ku
development
8080
true
```

真实凭据不要提交到源码仓库、公开示例或日志；这里都是演示值。

## 接口与路径

| 调用                        | 返回         | 文件缺失或失败时                                                      |
| --------------------------- | ------------ | --------------------------------------------------------------------- |
| `config.env()`              | 字符串值对象 | 读取当前源码旁的 `.env`；文件不存在返回空对象，读取或解析错误结束执行 |
| `config.env_file(path:str)` | 字符串值对象 | 读取指定文件；读取或解析错误结束执行                                  |
| `config.yaml(path:str)`     | `object!`    | 读取平面 YAML；失败返回可恢复错误                                     |

相对路径按当前 `.ku` 源文件所在目录解析，也支持绝对路径。文件必须为 UTF-8，上限为 1,000,000 字节。返回对象独立持有内容，不持有打开的配置文件句柄。重复调用会重新读取文件，没有自动重载、watcher、环境插值或合并优先级。

可选字段使用 `get_or`，例如 `env.get_or("APP_NAME", "Ku")`；直接读取缺失字段会报错。默认 `.env` 不存在时，`env.APP_NAME` 也不会自动得到默认值。

## .env 格式

- 每行一个 `KEY=value`，以第一个 `=` 分隔。
- 键以 ASCII 字母或 `_` 开头，其余为字母、数字、`_`。
- 忽略空行和以 `#` 开始的整行注释。
- 去除两端空白，支持单引号和双引号值。
- 引号内支持 `\n`、`\r`、`\t`、`\\`、`\"`、`\'` 转义。
- 值都是字符串，包括 `true`、`8080` 和空值；重复键保留最后一项。

不支持 `export KEY=value`、多行值、`${NAME}` 插值和行尾注释语法。值后面的 `# 文字` 会作为值的一部分。

## 平面 YAML 格式

只支持没有缩进的 `key: value`。键遵循与 `.env` 相同的 ASCII 标识符规则。忽略空行和整行 `#` 注释。值支持 `null`、小写 `true/false`、有符号整数、浮点数、带引号字符串和无引号字符串；重复键保留最后一项。

这是平面配置读取器，不是完整 YAML 实现。嵌套映射、数组、多行标量、tag 和 anchor 均不支持。不要使用行尾注释，它可能成为无引号字符串的一部分。需要嵌套数据时，可组合 [JSON](/zh-cn/docs/stdlib/json/) 与[文件读取](/zh-cn/docs/stdlib/fs/)。

## 错误与执行支持

`config.yaml` 失败返回 `{ domain: "config", code: "read_failed", message: ... }`，解析错误也使用这个 code。`env/env_file` 不返回 Result；需要可恢复的文件或解析错误时使用 `yaml`。只有 `yaml` 使用 `?`，不要给 `env/env_file` 加 `?`。

本模块支持 `ku run` 与默认打包 runner；同步 native C、native Task、LLVM 尚无这些配置 API。解释器中的 async 调用可经现有 blocking-worker 路径执行，不改变文件格式和返回类型。
