# Ku

[English](README.en.md) · [中文文档源码](web/src/content/docs/zh-cn/docs) · [English documentation source](web/src/content/docs/en/docs)

Ku 是一门开发中的编程语言，提供解释执行、静态检查、native C 编译、标准库和 VS Code 扩展。当前源码版本为 **0.0.19**，采用 [Apache-2.0](LICENSE) 许可证，归属声明见 [NOTICE](NOTICE)。

0.0.19 汇集多事件循环 HTTP、有限 native Task、异步 PostgreSQL 适配、服务恢复和已验证的运行时修复。它仍是实验版本：受控环境预演不代表已经完成真实生产灰度，个别负载的长尾延迟仍是已知问题。请先阅读 [版本说明](docs/v0.0.19.md) 和 [已知限制](docs/v0.0.19-known-issues.md)。

## 安装与第一个程序

从 [GitHub Releases](https://github.com/fengyanweb/ku/releases) 选择与系统和架构一致的安装包，核对随包 `SHA256SUMS`，解压后将包含 `ku`（Windows 为 `ku.exe`）的目录加入 `PATH`。保留包内伴随库和元数据，不要只复制一个可执行文件。

```sh
ku version
ku create HelloWorld
cd HelloWorld
ku check
ku run
```

仓库内的最小示例：

```ku
fn main() {
    println("Hello, Ku!")
}
```

```sh
ku run examples/hello.ku
ku check examples/json.ku
ku build --backend c --profile release examples/hello.ku -o hello
```

解释执行与 native C 的支持范围并不完全相同。数据库驱动需要 native 产物及对应客户端库；HTTP 多循环和 owner Task 指的是 native C 路径。不要把 `ku check` 成功等同于所有后端都能执行。

## 学习与使用

| 内容 | 入口 |
|---|---|
| 语言入门、类型、所有权和错误处理 | [中文指南](web/src/content/docs/zh-cn/docs) / [English guide](web/src/content/docs/en/docs) |
| 标准库和可运行示例 | [文档索引](web/src/content/docs/zh-cn/docs) / [examples](examples) |
| 语法及语义合同 | [syntax](docs/syntax.md) / [semantics](docs/semantics.md) |
| native HTTP、线程归属和配置 | [多事件循环](docs/multiloop-runtime.md) |
| Task 语义与后端支持范围 | [并发合同](docs/concurrency.md) |
| `ku service`、滚动更新和回退 | [部署示例](deploy/service/README.md) |
| `ku supervise`、进程资源限制 | [受控进程执行](docs/process-execution.md) |
| TCP/TLS 协议范围与验证身份 | [协议地基状态](docs/protocol-foundation.md) |
| 编辑器插件 | [VS Code 扩展](editors/vscode-ku/README.md) |
| 最新七语言固定负载对比 | [中文](docs/performance-v0.0.19.md) / [English](docs/performance-v0.0.19.en.md) |
| 0.0.19 交付范围 | [中文](docs/v0.0.19.md) / [English](docs/v0.0.19.en.md) |

常用命令为 `ku check`、`ku run`、`ku build`、`ku ir`、`ku package`、`ku service` 和 `ku supervise`。完整参数以当前安装的 `ku --help` 及各子命令帮助为准。

## 从源码构建

需要 Rust 1.89.0；native C 输出另需受支持的 C 编译器和所用原生库。

```sh
cargo +1.89.0 build --locked --release
./target/release/ku version
./target/release/ku run examples/hello.ku
```

Windows 使用 `target\release\ku.exe`。项目已有回归测试与 CI 夹具是源码的一部分；临时探针和重复构建包不应提交。

## 版本历史

- [0.0.19](docs/v0.0.19.md)：当前版本。
- [0.0.18](docs/v0.0.18.md) 与 [历史修订](docs/v0.0.18-r1.md)：保留原交付身份。
- [0.0.17](docs/v0.0.17.md)、[0.0.16](docs/v0.0.16.md)：早期版本。

历史性能数据只对应其原提交、输入、平台和限制，不表示所有工作负载都优于其他语言。当前官网代码的版本与已经部署的网站版本可能不同。
