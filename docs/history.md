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
```

`release/ku.exe` 始终是当前最新版本；`history/v*/ku.exe` 是对应历史版本快照。

## 当前记录

| 版本 | 状态 | 说明 |
| --- | --- | --- |
| 0.0.5 | 已归档 | Result / try-catch / ? 错误处理闭环 |
| 0.0.6 | 已归档 | 源码模块拆分和第一批 stdlib |

## 规则

- 每次版本发布都复制当前 `release/ku.exe` 和 `release/libku.rlib` 到 `history/vX.Y.Z/`。
- 如果未来 release 文件变多，按版本目录一起归档。
- 文档中必须说明该版本做了什么、没做什么、下一步做什么。
