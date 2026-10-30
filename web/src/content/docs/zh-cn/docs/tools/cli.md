---
title: '命令行工具'
description: 'Ku 常用命令、完整简写、参数作用域与 Runner 构建依赖。'
kuVersion: '0.0.19'
verifiedAt: '2026-10-29'
sources: ['src/cli.rs', 'docs/package.md']
---

先用 `ku -v` 查看版本，`ku --help` 查看当前安装的总帮助。

## 日常命令

```sh
ku -v
ku create hello
cd hello
ku check
ku run
```

项目入口由有效的 `ku.mod` 配置指定；省略路径不是在任意目录猜测 `hello.ku`。单文件也可直接运行：

```sh
ku ./hello.ku
```

| 命令                                            | 用途                                      |
| ----------------------------------------------- | ----------------------------------------- |
| `ku check main.ku`                              | 检查源码，不执行                          |
| `ku check --json main.ku`                       | 输出 JSON Lines 诊断，成功时静默          |
| `ku run main.ku`                                | 解释执行                                  |
| `ku build main.ku`                              | 默认 Debug / Runner 构建                  |
| `ku build --backend c --release main.ku -o app` | 编译并链接 native C 程序                  |
| `ku ir main.ku`                                 | 查看优化后的 Ku IR                        |
| `ku llvm empty.ku`                              | 输出实验性 LLVM 文本 IR，不生成可执行文件 |
| `ku template list`                              | 列出 basic、cli、http、json、fs、lib 模板 |
| `ku package resolve --locked`                   | 按项目 lock 固定依赖                      |
| `ku package pack`                               | 创建源码与依赖分发包，不是可执行程序      |
| `ku package gc`                                 | 清理未使用的包缓存                        |

## 完整形式与简写

别名只在相应命令的参数位置生效。

| 功能              | 完整形式                          | 简写或等价形式                              |
| ----------------- | --------------------------------- | ------------------------------------------- |
| 版本              | `ku --version`                    | `ku -v`、`ku -V`、`ku version`              |
| 总帮助            | `ku --help`                       | `ku -h`、`ku -help`、`ku help`              |
| 运行单文件        | `ku run ./hello.ku`               | `ku ./hello.ku`                             |
| 创建模板          | `ku create hello --template http` | `ku create hello -t http`                   |
| 初始化模板        | `ku init --template cli`          | `ku init -t cli`                            |
| 模板列表          | `ku template list`                | `ku create --list`                          |
| 输出路径          | `ku build --output PATH FILE`     | `ku build -o PATH FILE`                     |
| Release           | `--profile release`               | `--release`                                 |
| Debug             | `--profile debug`                 | `--debug`                                   |
| Small             | `--profile small`                 | `--small`                                   |
| Fast              | `--profile fast`                  | `--fast`                                    |
| LLVM 文本附加输出 | `--emit-llvm`                     | `--emit-ll`                                 |
| 构建详细日志      | `ku build --verbose FILE`         | `ku build -v FILE`                          |
| Runner 后端       | `--backend runner`                | `--backend interp`、`--backend interpreter` |
| C 后端            | `--backend c`                     | `--backend native-c`                        |
| LLVM 后端         | `--backend llvm`                  | `--backend ll`                              |

### `-v` 的作用域

`ku -v` 输出 Ku 版本；`ku build -v ./hello.ku` 显示构建日志，并不查看版本。profile 简写和 `--backend` 别名属于 `build` 选项，不是全局开关。

### 项目入口与路径

`ku run`、`ku check`、`ku build`、`ku build .` 可使用有效项目配置中的入口；`ku package` 对应子命令省略路径时使用当前目录项目上下文。`ku build --target host` 与默认的宿主目标选择等价。

单独执行 `ku` 不会默认运行项目；`ku run .` 也不是项目目录简写。隐式 `ku ./hello.ku` 只接受这一个路径，不接程序参数、`--offline` 或 `--locked`。需要依赖模式时使用 `ku run --offline ./hello.ku` 或 `ku run --locked ./hello.ku`，两种模式不能同时指定。

以 `-` 开头的文件使用显式相对路径，例如 `ku ./-hello.ku`。Windows 使用 `ku .\-hello.ku`；含空格或中文的路径用引号包住。当前不支持 `ku r`、`ku b`、`ku c`、`ku --v` 或程序 argv 透传。

## 构建什么产物

| 调用                                             | 产物和所需工具                                                                                     |
| ------------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| `ku build hello.ku`                              | 默认 Debug / Runner：生成 Rust wrapper，打包解释执行路径；需要与随包运行时匹配的 Rust 及系统链接器 |
| `ku build --backend c --release hello.ku -o app` | native C 可执行程序；需要受支持的 C 编译器、链接器及程序使用的原生库                               |
| `ku build --emit-c hello.ku`                     | 默认仍走 Runner，额外输出 C；不是 C-only 构建，也不能绕过 Runner 工具链不匹配                      |
| `ku build --backend llvm empty.ku`               | 当前为 LLVM 文本 IR，不是完整可执行链接路径                                                        |
| `ku package pack`                                | 源码分发包，与上面两种程序产物不同                                                                 |

`check`、`run` 与隐式运行不要求 Rust/C 编译器。Runner 只嵌入入口文件；含本地导入的程序运行时仍需原模块源码。native C 构建展开导入图，产物不依赖原 Ku 源码。默认输出在 `.ku/build/<profile>/`，请使用 CLI 报出的实际路径；Windows 输出名使用 `.exe`。

LLVM 示例中的 `empty.ku` 内容为 `fn main() {}`。该原型只支持部分语法和内建调用；`--backend llvm` 写出文本 IR 后返回非零，因为尚不支持可执行链接。

### 历史 `--native`

保留旧调用，但推荐在新代码中明确使用 `--backend c`：

```sh
ku build --native ./hello.ku
ku build --native ./hello.ku -o ./hello
```

第一条只生成 C 文件；第二条通过兼容分支编译和链接 C 程序。`--native` 必须位于 `build` 后的兼容解析位置，不能移到任意选项位置并当作无条件别名。

## 受控执行

`ku supervise` 是显式的完整程序 worker 管理模式，不会自动应用到普通 `run` 或 HTTP 请求。Linux 需要已委托的 cgroup v2，Windows 使用提交内存限制；macOS 原生不支持该隔离模式。配置、失败语义与未覆盖范围见[平台与进程限制](/zh-cn/docs/support/)。

## 常驻 native HTTP 服务

`ku service --help` 说明独立的常驻服务监督器。先构建 [native HTTP 示例](/zh-cn/docs/stdlib/http/)。目标程序必须支持 native 认证健康/排空协议；probe 与 control 地址须为不同、非零且端口空闲的 loopback 地址。

运行前准备私有普通文件 `service.token`，内容为 32–128 个 ASCII 字母或数字；Unix 下只能所有者访问（`chmod 600 service.token`）。使用自行生成的不可预测 token，不把 token 随源码提交，再执行：

```sh
ku service --probe-addr 127.0.0.1:18181 \
  --control-addr 127.0.0.1:19081 --control-token-file service.token \
  --startup-timeout-ms 5000 --probe-timeout-ms 250 \
  --probe-interval-ms 1000 --failure-threshold 3 \
  --drain-timeout-ms 10000 --restart-limit 5 --restart-window-ms 60000 \
  --backoff-ms 100 --max-backoff-ms 5000 -- ./native-server
```

上述数值显式给出了监督器默认值。时长单位为毫秒，不能为 0。`--failure-threshold` 为 1–100，`--restart-limit` 为 1–1000；backoff 不能超过 max-backoff，probe-timeout 不能超过 startup-timeout。`--` 后接可执行文件及参数，Windows 改为实际 `.exe` 路径。

control listener 通过 `X-Ku-Control-Token` 认证暴露 `GET /status`、`GET /metrics`、`POST /stop`。它管理单个服务实例的健康、准入/排空与有界重启，不是请求代理或数据库重试机制。此模式将被管理程序 stdout/stderr 接到 null device，不保留应用日志，应按控制指标观察状态。

不带内存选项时普通模式不强制 OS 内存上限。显式内存模式必须同时指定 `--worker-memory-bytes` 与 `--total-memory-bytes`；Linux 还需委派的 `--cgroup-root`，先阅读[平台限制](/zh-cn/docs/support/)。native macOS 拒绝此硬限制模式，但支持普通监督。指标区分当前状态与失败；健康探测成功不证明业务 SLO 或已有稳定生产后端。
