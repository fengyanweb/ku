# CLI、SDK 与构建工具链

[English](cli-runtime-bundle.en.md) · [命令行完整参考](../web/src/content/docs/zh-cn/docs/tools/cli.md)

本文说明安装包的目录布局、构建依赖和升级约定。当前源码普通 `ku build` 默认使用原生 C；已发布 0.0.20 安装包仍默认使用 Runner。下文 Runner 诊断与构建使用显式 `--backend runner`，不改变 `ku run`。

## 为什么会有 E0514

Rust 的 `rlib` 含编译器专有元数据，不是跨 rustc 版本的稳定分发接口。[E0514 官方说明](https://doc.rust-lang.org/error_codes/E0514.html)要求使用兼容的编译器及依赖。Cargo 的 [rust-version](https://doc.rust-lang.org/cargo/reference/rust-version.html) 是最低支持版本，不是工具链锁。用户项目中的 Rustup 文件、override 或系统默认有自己的[选择优先级](https://rust-lang.github.io/rustup/overrides.html)，不能依赖它们碰巧选择 Ku 的发行编译器。

## 安装包清单与工具链

保留安装包完整目录，包括：

- `ku` / `ku.exe`；
- `ku-runtime.json` 与 `ku-runtime-deps.json`；
- 精确清单引用的 `libku.rlib`、`deps` 和其他伴随库；
- native TLS 等原有 pack、许可证与安装说明。

从真实 CLI 路径定位安装根，再按 manifest 中的相对路径检查同批 CLI、运行时和依赖的大小与 SHA。不会按 mtime 选较新库、向祖先 target 目录兜底，或从当前项目借另一份 Ku 库。丢失 manifest、非法路径、缺文件、损坏和混包都应明确失败。完整搬迁归档可以；只搬一个可执行文件不能保留 Runner 构建能力。

发行工具链 **T=1.99.1** 由 [`scripts/release-toolchain.json`](../scripts/release-toolchain.json) 指定。源码 MSRV 与发行 T 分开记录；升级 T 时需要同批重建 CLI、rlib、依赖及受影响的 native/TLS pack。

显式 Runner 构建前核对编译器完整 release/commit/host 与运行时目标 ABI。系统已有完整身份匹配的独立 rustc 可以使用；不匹配时选择 Rustup 已安装的精确 T。probe 与实际编译使用同一个已定位编译器，不再 probe 一份、从 PATH 编译另一份。普通原生 C 构建不读取这些 rlib，也不要求安装 T。

用户显式设置 `KU_RUNNER_RUSTC` 时必须提供编译器**绝对路径**，优先于自动选择；身份不匹配会报错，不静默忽略。不改全局 default、目录 override、项目 `rust-toolchain.toml`。诊断子进程禁用 Rustup 自动安装，安装只由显式命令请求。

## Windows 诊断

Windows MSVC 构建需要 Visual Studio Build Tools 的 C++ 工具和 Windows SDK；安装 Rust 工具链不包含这些系统组件。

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

诊断命令：

```powershell
ku doctor
ku doctor --json
ku doctor --backend runner
ku doctor --backend c
```

诊断默认只读。未请求的可选后端缺失报告 WARN；显式检查的后端依赖不满足时非零退出。`--json` 的标准输出只含结构化诊断。

找不到匹配工具链而 Rustup 可用时，明确安装并再运行原命令：

```powershell
ku doctor --install-runner-toolchain
ku build --backend runner .\hello.ku
```

安装动作只请求本包所需的精确工具链，不更改系统默认。原命令输出位置以 CLI 报出的路径为准，不是固定的当前目录 `hello.exe`。也可指定：

```powershell
ku build --backend runner .\hello.ku -o ".\dist\hello.exe"
& ".\dist\hello.exe"
```

当前源码的默认原生构建：

```powershell
ku build --release .\hello.ku -o ".\dist\hello-native.exe"
& ".\dist\hello-native.exe"
```

在当前源码与旧版安装包中都明确选择 C，可使用：

```powershell
ku build --backend c --release .\hello.ku -o ".\dist\hello-native.exe"
& ".\dist\hello-native.exe"
```

## Native C 的构建与部署依赖

当前源码省略 `--backend` 与 `--backend c` 均使用目标平台的 C 编译器和链接器；`--backend native-c` 是同义写法。缺少编译器、依赖或遇到不支持的程序时明确报错，不转为 Runner。普通 C 程序构建不读取 `libku.rlib`，也不为生成程序调用 Rust 编译器；`ku run` 和直接执行 `.ku` 的行为不因此改变。使用 `ku doctor --backend c` 检查 C 工具链；该检查不代表数据库库和其他可选原生依赖已经齐备。

native `net.client` 启用 TLS 时，保留安装包中与 `ku` 相邻的 `native-tls/v1/<target>/`。CLI 自动定位同目标的 pack，并检查 manifest、header 和静态库；完整搬迁安装目录不需要重新设置路径。预构建 TLS 库在链接时进入程序，部署后不依赖 Ku 安装目录或 TLS pack，但仍依赖目标操作系统的 CRT 和系统网络库。

含可选压缩模块的配套包将依赖放在 `native-compression/v1/<target>/`，供 `bytes`、`zstd`、`gzip` 与 `deflate` 的同步 C 路径自动发现；模块规则见[原生压缩文档](compression.md)。只有使用相应 codec 的程序才链接它。旧安装包中没有这些模块时，不应将另一个版本的库或包改名混入。

数据库客户端动态库不包含在 Ku 安装包内。PostgreSQL 使用绝对目录 `KU_PG_LIB`；MySQL/MariaDB 使用 `KU_MYSQL_LIB` 和对应头文件。库的 CPU 架构、目标 ABI 和客户端家族必须与构建目标一致。构建成功后，部署环境仍须提供匹配的客户端动态库及其依赖；`--static` 不能替代这些依赖检查。不要将 C 编译成功、交叉编译成功或源码检查当成目标机器运行通过。

## 更新与失败处理

新包解压到独立空目录，完整验证后再切换 PATH。不要覆盖组合新 EXE 与旧 rlib；旧目录保留可回退。安装目录可以只读，程序构建的临时文件不写安装根。Windows 被占用文件先停止其所属 Ku 进程再切换，避免留下半更新目录。

没有匹配工具链、链接器或目标标准库时，在编译前明确报告缺口；不能把“找到 rustc”当 Runner ready。缺失/损坏库应恢复完整已验证包，不从其他版本复制单个 rlib。如果仍产生 E0514，保留原始 rustc 详细错误与 doctor 身份，继续按混包/身份不一致排查，不循环切换编译器或增加超时。

子命令帮助可用 `ku help build`、`ku build --help`、`ku run --help` 等公开子命令入口。帮助不执行程序、初始化项目或构建；未知命令和选项返回非零。原有 `service` / `supervise` 专属帮助继续保留。
