# Ku 解释器历史

这个目录记录每个公开版本对应的解释器程序，避免只保留最新 `release/ku.exe`。

## 文件位置

```txt
history/
  v0.0.5/
    ku.exe
    libku.rlib
  v0.0.6/
    ku.exe
    libku.rlib
  v0.0.7/
    ku.exe
    libku.rlib
  v0.0.8/
    ku.exe
    libku.rlib
  v0.0.9/
    ku.exe
    libku.rlib
  v0.0.10/
    ku.exe
    libku.rlib
  v0.0.11/
    ku.exe
    libku.rlib
  v0.0.12/
    ku.exe
    libku.rlib
```

`release/ku.exe` 始终是当前最新版本；`history/v*/ku.exe` 是对应历史版本快照。

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
| 0.0.14 | 当前 | create/init/template 项目模板、HTTP status helper、匿名 fn handler、VS Code/release 同步 |

## 自动化

发布或本地更新解释器时，在项目根目录运行：

```powershell
.\scripts\archive-release.ps1
```

脚本会自动执行：

```powershell
cargo build --release
Copy-Item -LiteralPath target\release\ku.exe -Destination release\ku.exe -Force
Copy-Item -LiteralPath target\release\libku.rlib -Destination release\libku.rlib -Force
Copy-Item -LiteralPath target\release\ku.pdb -Destination release\ku.pdb -Force
Copy-Item -LiteralPath target\release\ku.exe -Destination history\v$version\ku.exe -Force
Copy-Item -LiteralPath target\release\libku.rlib -Destination history\v$version\libku.rlib -Force
Copy-Item -LiteralPath target\release\ku.pdb -Destination history\v$version\ku.pdb -Force
```

其中 `$version` 来自 `Cargo.toml` 的 `package.version`，例如当前版本会写入 `history\v0.0.14\`。

只检查产物和版本路径：

```powershell
.\scripts\archive-release.ps1 -CheckOnly -SkipBuild
```

如果只想手动更新当前本地解释器，可以运行：

```powershell
cargo build --release
Copy-Item -LiteralPath target\release\ku.exe -Destination release\ku.exe -Force
Copy-Item -LiteralPath target\release\libku.rlib -Destination release\libku.rlib -Force
Copy-Item -LiteralPath target\release\ku.pdb -Destination release\ku.pdb -Force
Copy-Item -LiteralPath target\release\ku.exe -Destination history\v0.0.14\ku.exe -Force
Copy-Item -LiteralPath target\release\libku.rlib -Destination history\v0.0.14\libku.rlib -Force
Copy-Item -LiteralPath target\release\ku.pdb -Destination history\v0.0.14\ku.pdb -Force
```

## 规则

- 每次版本发布都复制当前 `release/ku.exe`、`release/libku.rlib` 和可用的 `release/ku.pdb` 到 `history/vX.Y.Z/`。
- 如果未来 release 文件变多，按版本目录一起归档。
- 文档中必须说明该版本做了什么、没做什么、下一步做什么。
