# Ku 发布与历史

当前发布合同按操作系统/CPU target 隔离，避免 Windows、Linux 和 macOS 产物互相覆盖。一个 bundle 只对应一个 target，不宣称存在三系统通用二进制。

## 文件位置

```txt
release/
  <target>/
    ku[.exe]
    libku.rlib
    deps/                          # Rust 运行器编译依赖：RLIB 与匹配平台的 proc-macro 动态库
    ku-language-<version>.vsix
    ku.pdb                         # 仅 Windows，存在时包含
    native-tls/v1/<target>/
      manifest.kutls
      include/ku_native_tls.h
      lib/<target-specific archive>

history/
  v<version>/
    <target>/                      # 与 release/<target>/ 相同的完整 bundle
```

`target` 是精确 Rust triple，当前支持 `x86_64-pc-windows-msvc`、`x86_64-unknown-linux-gnu` 和 `aarch64-apple-darwin`。仓库中早期版本仍可能使用 `release/ku.exe` 或 `history/vX.Y.Z/ku.exe` 旧布局；它们只是历史记录，不是新发布脚本的输出合同。

## 当前记录

| 版本 | 状态 | 说明 |
| --- | --- | --- |
| 0.0.5 | 已归档 | Result / try-catch / ? 错误处理闭环 |
| 0.0.6 | 已归档 | 源码模块拆分和第一批 stdlib |
| 0.0.7 | 发布时归档 | stdlib Result、package 草案、IR 草案 |
| 0.0.8 | 发布时归档 | 引用捕获闭包前置、typed CFG IR 草案 |
| 0.0.9 | 发布时归档 | typed temp IR、stdlib ABI metadata、package lock、native C 原型 |
| 0.0.10 | 发布时归档 | 运行时 capture map、Result CFG、package lock 依赖、C 后端 if/while/int 子集 |
| 0.0.11 | 发布时归档 | Result ABI、try/finally 错误 CFG、file package 依赖缓存、match guard 诊断 |
| 0.0.12 | 已归档 | 嵌套 match 模式、std.http Response API、HTTP client 连接复用和 helper、fs 写入、严格 bool 条件、native C main wrapper |
| 0.0.13 | 已归档 | build 命令入口、ku.mod main/out、std root 小写导入诊断、std.time 边界、VS Code/release 同步 |
| 0.0.14 | 已归档 | create/init/template 项目模板、HTTP status helper、匿名 fn handler、VS Code/release 同步 |
| 0.0.15 | 已归档 | 对象解构、HTTP service 调用严格化、示例重写、clone/IR/native 路线同步 |
| 0.0.16 | 历史版本 | native 标准库、统一数据库 client、package/registry 与 target-scoped 发布合同 |
| 0.0.17 | 当前实验版本 | 同步 `&` 只读借用、Stage3 braced `while` 与跨平台测试工具修复；验收状态见 [版本记录](v0.0.17.md) |

## 自动化

脚本要求 PowerShell 7。先做不发布 bundle 的完整构建/合同检查：

```powershell
pwsh -NoLogo -NoProfile -File scripts\package-release.ps1 -CheckOnly
pwsh -NoLogo -NoProfile -File scripts\archive-release.ps1 -CheckOnly
```

生成当前 host target 的 release bundle：

```powershell
pwsh -NoLogo -NoProfile -File scripts\package-release.ps1
```

从当前源码重新构建，并同时更新 release bundle 与不可变历史快照（不必先执行上一条命令）：

```powershell
pwsh -NoLogo -NoProfile -File scripts\archive-release.ps1
```

`package-release.ps1` 使用当前 host 对应的显式 Cargo target 目录，构建固定版本 TLS pack 与 lockfile-backed VSIX，并在私有 staging 中校验文件集、目标架构和 pack 合同后，以 per-target 单写者锁、完整目录切换和 journal 崩溃恢复发布 `release/<target>/`。替换既有目录需要两次目录移动，无锁读者在切换窗口可能短暂看不到 current target，不能把它称为原子可见。`archive-release.ps1` 调用同一构建链路，重新构建后同时更新 release 并发布 `history/v<version>/<target>/`，不会直接复制或信任原有 release；历史目标已存在时拒绝覆盖。两条 `-CheckOnly` 命令各自执行实际构建与验证，但不发布 release/history bundle；通常运行 archive 的检查即可覆盖该完整链路，无需为一次验收重复构建。没有跳过构建的 `-SkipBuild` 逃生门。

`libku.rlib` 不是普通 `ku build` 的完整依赖集：bundle 还携带同次私有 Cargo 构建的 `deps/`，包含 Rust 运行器所需的 RLIB 和匹配平台的 proc-macro 动态库。脚本固定文件名、大小、SHA-256 与对象格式，拒绝缺失、额外或变化的依赖。普通 build 的隔离 consumer 清除开发库搜索环境，固定 Rust 1.89.0；先隐藏 `deps` 并要求缺库失败，再恢复依赖完成带本地 import 的普通 build，在保留源码时运行，并验证隐藏编译依赖后仍能运行；另用无 import 的独立 runner 验证移走源码后运行。默认 runner 只嵌入主文件，含 import 时仍需要原导入源码；需要包含完整 import graph 的独立部署请使用 `ku build --native`。该门槛独立于 native TLS consumer，避免只验证 native build 而遗漏默认 runner 路径。用户执行默认 `ku build` 仍需匹配的 Rust 工具链及系统 linker。

GitHub 安装包使用 `ku-v<version>-<Rust triple>.tar.gz`，以 [Releases](https://github.com/fengyanweb/ku/releases) 中实际列出的版本与 target 资产为准。外层下载资产附带 `README-INSTALL.md`、`RELEASE.json`、逐文件 `SHA256SUMS`、`THIRD_PARTY.json` 与 `THIRD_PARTY/` notices；并列的 `.tar.gz.manifest.json` 记录归档整体和文件清单的 hash。内部严格 bundle 不包含 `ku-registry` 服务端。`Native three-OS gate` 上传的 `native_ci-*` 是源码无关 native 验收样例，不是 Ku CLI 安装包；也不能把 workflow 已配置或某个旧提交通过等同于新版本发布成功。

## 规则

- 每个 target 必须使用匹配的 host/toolchain 独立构建和验收；不从一个 host bundle 推导其他两个系统已通过。
- `release/<target>/` 以 per-target 锁串行替换为同 target 的完整已验 bundle，并用 journal 恢复中断切换；替换窗口对无锁读者不保证 current target 始终可见。`history/v<version>/<target>/` 不可变，已存在时不覆盖。
- bundle 目录和内容必须是有界的普通文件/目录，不接受 symlink/reparse point。
- 文档中必须说明该版本做了什么、没做什么、下一步做什么。
