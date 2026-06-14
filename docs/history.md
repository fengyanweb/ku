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
| 0.0.12 | 本次发布待归档 | 嵌套 match 模式、std:http 显式导入、native C main wrapper |

## 自动化

发布时在项目根目录运行：

```powershell
.\scripts\archive-release.ps1
```

只检查产物和版本路径：

```powershell
.\scripts\archive-release.ps1 -CheckOnly -SkipBuild
```

## 规则

- 每次版本发布都复制当前 `release/ku.exe` 和 `release/libku.rlib` 到 `history/vX.Y.Z/`。
- 如果未来 release 文件变多，按版本目录一起归档。
- 文档中必须说明该版本做了什么、没做什么、下一步做什么。
