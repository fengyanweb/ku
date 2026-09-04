# Ku 发布与历史

当前发布合同按操作系统/CPU target 隔离，避免 Windows、Linux 和 macOS 产物互相覆盖。一个 bundle 只对应一个 target，不宣称存在三系统通用二进制。

## 文件位置

```txt
release/
  <target>/
    ku[.exe]
    libku.rlib
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
| 0.0.16 | 当前 | native 标准库、统一数据库 client、package/registry 与 target-scoped 发布合同 |

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

生成 release bundle 后再发布不可变历史快照：

```powershell
pwsh -NoLogo -NoProfile -File scripts\archive-release.ps1
```

`package-release.ps1` 使用当前 host 对应的显式 Cargo target 目录，构建固定版本 TLS pack 与 lockfile-backed VSIX，并在私有 staging 中校验文件集、目标架构和 pack 合同后，以 per-target 单写者锁、完整目录切换和 journal 崩溃恢复发布 `release/<target>/`。替换既有目录需要两次目录移动，无锁读者在切换窗口可能短暂看不到 current target，不能把它称为原子可见。`archive-release.ps1` 复验该 bundle，再以不可变目标发布 `history/v<version>/<target>/`；该目标已存在时拒绝覆盖。`-CheckOnly` 仍执行实际构建与验证，但不发布 release/history bundle；没有跳过构建的 `-SkipBuild` 逃生门。

## 规则

- 每个 target 必须使用匹配的 host/toolchain 独立构建和验收；不从一个 host bundle 推导其他两个系统已通过。
- `release/<target>/` 以 per-target 锁串行替换为同 target 的完整已验 bundle，并用 journal 恢复中断切换；替换窗口对无锁读者不保证 current target 始终可见。`history/v<version>/<target>/` 不可变，已存在时不覆盖。
- bundle 目录和内容必须是有界的普通文件/目录，不接受 symlink/reparse point。
- 文档中必须说明该版本做了什么、没做什么、下一步做什么。
