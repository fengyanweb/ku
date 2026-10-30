# CLI 与 Runner 安装包修复候选

[English](cli-runtime-bundle.en.md) · [命令行完整参考](../web/src/content/docs/zh-cn/docs/tools/cli.md)

本说明对应 `codex/v0.0.19-cli-bundle-fix`，基于已发布 0.0.19 的产品代码准备独立修复。**尚未发布，不覆盖原 `v0.0.19` 标签或安装资产**。下文新增的 `doctor` 和统一子命令帮助仅用于修复候选；原公开包可能不识别这些入口。正式安装包身份、实际通过/失败检查以本次交付记录为准，本文不是验收收据。

## 已有命令与产物

```sh
ku -v
ku create hello
cd hello
ku check
ku run
```

单文件用 `ku ./hello.ku`，等价于 `ku run ./hello.ku`，只接收一个源路径。项目入口省略依赖有效 `ku.mod`；单独 `ku` 不运行项目，`ku run .` 不是项目目录简写。`ku -v` 是版本，`ku build -v FILE` 是详细构建日志。全部简写、profile、后端别名及参数位置见命令行参考。

| 调用 | 实际含义 |
| --- | --- |
| `ku build hello.ku` | 默认 Debug / Runner：Rust wrapper 打包解释执行路径 |
| `ku build --backend c --release hello.ku -o hello` | C 原生编译与链接，Windows 输出使用 `hello.exe` |
| `ku build --emit-c hello.ku` | 默认仍为 Runner，另外输出 C，不解决 Runner Rust 不匹配 |
| `ku build --backend llvm hello.ku` | 文本 LLVM IR，不是完整可执行链接路径 |
| `ku package pack` | 源码/依赖分发包，不是程序可执行文件 |

历史 `ku build --native FILE` 仅输出 C；在对应兼容位置同时带 `-o` 时编译 C 程序，不是任意位置都合法的普通后端别名。`ku check` 成功也不等于所有后端可以编译或执行。

## 为什么会有 E0514

原 Runner 构建会从 CLI 附近收集 `libku.rlib` 与 `deps/libku-*.rlib`，按文件时间选库，并从 PATH 调用 `rustc`。这样可能用系统 1.96 编译器消费由 1.89 编译的 Ku 库；开发目录的祖先依赖兜底也可能掩盖缺包。

Rust 的 `rlib` 含编译器专有元数据，不是跨 rustc 版本的稳定分发接口。[E0514 官方说明](https://doc.rust-lang.org/error_codes/E0514.html)要求使用兼容的编译器及依赖。Cargo 的 [rust-version](https://doc.rust-lang.org/cargo/reference/rust-version.html) 是最低支持版本，不是工具链锁。用户项目中的 Rustup 文件、override 或系统默认有自己的[选择优先级](https://rust-lang.github.io/rustup/overrides.html)，不能依赖它们碰巧选择 Ku 的发行编译器。

仅凭报错中的库路径不能确定具体用户包来自哪个构建，也不能直接认定它硬编码了开发机路径。具体资产来源仍需实际 `ku.exe` 路径、版本及 SHA 证据。

## 候选安装包和工具链政策

保留安装包完整目录，包括：

- `ku` / `ku.exe`；
- `ku-runtime.json` 与 `ku-runtime-deps.json`；
- 精确清单引用的 `libku.rlib`、`deps` 和其他伴随库；
- native TLS 等原有 pack、许可证与安装说明。

从真实 CLI 路径定位安装根，再按 manifest 中的相对路径检查同批 CLI、运行时和依赖的大小与 SHA。不会按 mtime 选较新库、向祖先 target 目录兜底，或从当前项目借另一份 Ku 库。丢失 manifest、非法路径、缺文件、损坏和混包都应明确失败。完整搬迁归档可以；只搬一个可执行文件不能保留 Runner 构建能力。

发行工具链由 [`scripts/release-toolchain.json`](../scripts/release-toolchain.json) 统一指定。本次热修复保留已验证的 **T=1.89.0**，与原 native/TLS 链接合同一致；源码 MSRV 和发行 T 分开记录。未来升级 T 时要同批重建 CLI、rlib、依赖和受影响 pack，并重新验证消费包。不是要求用户永久降低全局 Rust 默认版本。

候选在编译前核对编译器完整 release/commit/host 与运行时目标 ABI。系统已有完整身份匹配的独立 rustc 可以使用；不匹配时选择 Rustup 已安装的精确 T。probe 与实际编译使用同一个已定位编译器，不再 probe 一份、从 PATH 编译另一份。

用户显式设置 `KU_RUNNER_RUSTC` 时必须提供编译器**绝对路径**，优先于自动选择；身份不匹配会报错，不静默忽略。候选不改全局 default、目录 override、项目 `rust-toolchain.toml`，也不删除其他版本或用户 Cargo 缓存。诊断子进程禁用 Rustup 自动安装，安装只由显式命令请求。

## Windows 诊断与修复候选

先确认实际命令和工具链，不复制所有环境变量或项目内容：

```powershell
Get-Command ku -All
where.exe ku
ku -v
rustc -vV
rustup show active-toolchain
rustup toolchain list
```

没有 Rust/Rustup 时对应命令会失败，这不是 Ku 解释执行必需依赖。真实库路径可以包含用户名；分享前按需隐藏个人路径。

在新候选包中：

```powershell
ku doctor
ku doctor --json
ku doctor --backend runner
ku doctor --backend c
```

这些操作默认只读。基础安装与可选后端分开：没有 C 编译器可为 WARN，不把整个 Ku 判不可用；显式检查 Runner/C 时，该后端依赖不满足会非零退出。JSON 模式标准输出只含结构化诊断；普通帮助不是 JSON 诊断流。

找不到匹配工具链而 Rustup 可用时，明确安装并再运行原命令：

```powershell
ku doctor --install-runner-toolchain
ku build .\hello.ku
```

安装动作只请求本包所需的精确工具链，不更改系统默认；安装失败不给虚假的 ready 标记。原命令输出位置以 CLI 报出的路径为准，不是固定的当前目录 `hello.exe`。也可指定：

```powershell
ku build .\hello.ku -o ".\dist\hello.exe"
& ".\dist\hello.exe"
```

C 后端独立验证，不拿它替代默认 Runner 原命令的成功证据：

```powershell
ku build --backend c --release .\hello.ku -o ".\dist\hello-native.exe"
& ".\dist\hello-native.exe"
```

## 更新与失败处理

新包解压到独立空目录，完整验证后再切换 PATH。不要覆盖组合新 EXE 与旧 rlib；旧目录保留可回退。安装目录可以只读，程序构建的临时文件不写安装根。Windows 被占用文件先停止其所属 Ku 进程再切换，避免留下半更新目录。

没有匹配工具链、链接器或目标标准库时，在编译前明确报告缺口；不能把“找到 rustc”当 Runner ready。缺失/损坏库应恢复完整已验证包，不从其他版本复制单个 rlib。如果仍产生 E0514，保留原始 rustc 详细错误与 doctor 身份，继续按混包/身份不一致排查，不循环切换编译器或增加超时。

候选统一帮助可用 `ku help build`、`ku build --help`、`ku run --help` 等公开子命令入口。帮助不执行程序、初始化项目或构建；未知命令和选项返回非零。原有 `service` / `supervise` 专属帮助继续保留。

## 与性能交付分开验收

该可用性修复基于发布产品代码，不自动采用整个性能开发分支。最新性能记录仍对应 `cbd158d` 候选及其原始输入：80 个有效固定窗口、1,209,000 正确响应；16 个 CPU/内存目标仅 1 项达到，15 项未达。JSON、严格 PG 与稳态 memory.current 差距、容量上界及 CPU legacy-blocking / Task 合同差异仍保留。

本次安装包消费、doctor 和帮助检查不能代替“超越 Rust”的性能验收；已有有效 80 窗/V/C2 不因 CLI 文档修改整套重跑。官网代码更新与打包也不等于已部署，旧公开安装包不会因文档更新自动获得这些修复。
