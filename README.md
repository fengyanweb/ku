# Ku 编程语言

[English](README.en.md) · [官网](https://ku-lang.com/) · [中文文档](https://ku-lang.com/zh-cn/docs/getting-started/) · [安装与下载](https://ku-lang.com/zh-cn/docs/install/)

Ku 提供解释器、静态检查、原生 C 编译、标准库和 VS Code 扩展。当前正式发行版本为 **0.0.20**，采用 [Apache-2.0](LICENSE) 许可证；第三方归属见 [NOTICE](NOTICE)。语言和运行时仍在持续完善，正式发行不代表所有后端、容量和长期稳定性目标均已完成。

本版整理已采用的 HTTP／Task／JSON 和 PostgreSQL 修复，更新 CLI、匹配 SDK 及 VS Code 扩展。具体变化见 [0.0.20 发布说明](docs/v0.0.20.md)，平台与运行限制见 [已知问题](docs/v0.0.20-known-issues.md)。

## 安装和运行

从 [v0.0.20 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.20) 下载匹配操作系统和架构的安装包，按 `SHA256SUMS` 校验后解压，将其目录加入 `PATH`。保留包内 CLI、SDK、依赖和元数据的相对位置。

```sh
ku -v
ku create HelloWorld
cd HelloWorld
ku check
ku run
```

```ku
fn main() {
    println("Hello Ku")
}
```

`ku run` 和 `ku check` 不要求安装 Rust。默认 Runner `ku build` 需要与 SDK 完整编译器身份匹配的 Rust **1.99.1** 及系统链接器；预编译 `libku.rlib` 不能跨任意 rustc 版本使用。原生 C 构建需要目标 C 编译器和程序使用的原生库：

```sh
ku build --backend c --release hello.ku -o hello
```

源码开发固定使用 Rust 1.99.1：`cargo +1.99.1 build --locked --release`。完整兼容规则见 [CLI 与 SDK](docs/cli-runtime-bundle.md)。

不同后端的支持范围不同。数据库驱动使用原生路径及对应客户端库；Task、HTTP 和 JSON 的覆盖范围见 [并发文档](docs/concurrency.md) 和 [官网标准库参考](https://ku-lang.com/zh-cn/docs/stdlib/)。

0.0.19 的历史性能记录仍见 [原始说明](docs/performance-v0.0.19.md)，不代表本版完成了新的跨语言排名或全部性能目标。[0.0.19](docs/v0.0.19.md)、[0.0.18](docs/v0.0.18.md) 和更早版本的说明保留原身份。

## 常用命令

| 用途 | 命令 |
| --- | --- |
| 版本 | `ku -v`、`ku --version` |
| 帮助 | `ku -h`、`ku --help` |
| 新建项目 | `ku create HelloWorld` |
| 解释执行 | `ku run hello.ku` |
| 静态检查 | `ku check hello.ku` |
| JSON 诊断 | `ku check --json hello.ku` |
| 默认 Runner 构建 | `ku build --release hello.ku -o hello` |
| 原生 C 构建 | `ku build --backend c --release hello.ku -o hello` |
| SDK 检查 | `ku doctor --backend runner` |

在包含 `ku.mod` 的项目中，`run`、`check` 和 `build` 可以省略入口路径。完整参数以安装版本的子命令帮助为准。

## 文档和示例

- [语言指南与标准库](https://ku-lang.com/zh-cn/docs/getting-started/)：所有权、错误处理、HTTP、JSON、文件、时间和数据库。
- [可运行示例](examples) · [语法合同](docs/syntax.md) · [语义合同](docs/semantics.md)。
- [Task 和后端覆盖](docs/concurrency.md) · [CLI／SDK](docs/cli-runtime-bundle.md)。
- [网络与数据库协议](docs/protocol-foundation.md)：连接选项、TLS 和已验证的平台支持。
- [VS Code 扩展](editors/vscode-ku/README.zh-CN.md)：语法高亮、补全、格式化和真实 CLI 诊断。
- [本版发布说明](docs/v0.0.20.md) · [已知问题和平台要求](docs/v0.0.20-known-issues.md)。

## 历史版本

[0.0.19](docs/v0.0.19.md)、[0.0.18](docs/v0.0.18.md)、[0.0.17](docs/v0.0.17.md) 及更早版本保留原版本说明。历史性能数据仅适用于其记录的源码、负载、平台和资源条件。

## 反馈

通过 [GitHub Issues](https://github.com/fengyanweb/ku/issues) 或邮件 [kagura.gc@qq.com](mailto:kagura.gc@qq.com) 反馈问题。请附 Ku 版本、操作系统、最小复现程序和错误输出。

## 赞赏支持

欢迎通过[爱发电](https://afdian.com/a/ku-lang)或微信赞赏支持 Ku 的开发与文档维护。赞赏完全自愿，不影响使用。

<a href="web/public/images/wechat-donation.png"><img src="web/public/images/wechat-donation.png" alt="Ku 项目微信赞赏码" width="220"></a>
