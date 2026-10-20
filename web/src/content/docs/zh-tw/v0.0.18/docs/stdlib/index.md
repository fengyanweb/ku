---
title: 標準函式庫概覽
description: 按用途查找 Ku 標準函式庫、基礎介面與不同後端的使用邊界。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - src/stdlib/metadata.rs
  - src/stdlib/mod.rs
  - docs/syntax.md
  - docs/protocol-foundation.md
slug: zh-tw/v0.0.18/docs/stdlib
---

本文檔對應 v0.0.18 實驗預發行版的凍結範圍；實際公開狀態與下載資產以 GitHub Release 為準。這裡是經過原始碼核對的核心入口，不是全部 API 的完整參考。具體行為遵循[語法合同](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md)，後端範圍見[支援矩陣](/zh-tw/v0.0.18/docs/support/)。

## 匯入模組

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`、`http`、`config`、`task`、`pg`、`redis`、`mysql`、`bytes`、`net` 需要顯式匯入。部分歷史模組仍可直接呼叫；新範例優先顯式匯入，使依賴清楚。`std.task` 是內部觀測與壓力測試命名空間，不作為普通業務併發 API。

| 模組                                 | 用途                                 | 需要注意                                               |
| ------------------------------------ | ------------------------------------ | ------------------------------------------------------ |
| `std.string` / 字串方法              | Unicode 長度、位元組長度、查找、切片 | native `trim/lower/upper` 尚無 Unicode 實作            |
| `std.array` / 陣列方法               | 長度、取值、創建新陣列、map          | 索引失敗與可恢復 `try_get` 不同                        |
| `std.object`                         | 顯式寬松讀取 `get_or`                | 普通缺鍵索引不會預設回傳 null                          |
| [std.json](/zh-tw/v0.0.18/docs/stdlib/json/) | 嚴格 JSON 編碼與解碼                 | native Task 與 LLVM 不支援                             |
| `std.fs`                             | 讀寫文件、存在性、目錄讀取           | 相對路徑按目前 `.ku` 文件目錄解析                      |
| `std.time`                           | 時間、日期、時長、sleep              | 同步與 Task 路徑等待方式不同                           |
| `std.http`                           | HTTP 客戶端、服務與響應              | 保留後端和生命週期差異；不是通用 Task 網絡完成聲明     |
| `std.config`                         | 環境與配置讀取                       | 不應把實際憑據寫進原始碼                               |
| `std.pg` / `std.redis` / `std.mysql` | 統一 client 與有界連接池             | native-only；實庫驗收和 TLS 邊界需單獨核對             |
| `std.bytes` / `std.net`              | 二進制資料、TCP client 與可選 TLS    | 目前 generated native C 路徑；目標 TLS pack 有額外要求 |
| `std.lexer` / `std.parser`           | 編譯器調試輔助                       | 回傳調試摘要，不是完整 Token / AST 公共結構            |

## 常用簽名

以下是文件中的介面簽名，`&` 表示已支援的唯讀參數模式，`T!` 表示可恢復錯誤結果：

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

`"中😀".len()` 為 2，`.byte_len()` 為 7；長度計 Unicode 標量值，切片下標也按字符而不是 UTF-8 位元組。`array.push` 與 `array.concat` 回傳新陣列，不原地修改輸入。

## 文件讀取範例

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

在源文件同目錄準備 UTF-8 的 `message.txt`。不存在或讀取失敗時回傳錯誤；該範例不是不依賴輸入文件的自包含程序。`try_read` / `try_write` 是保留的同型別兼容別名。

`read_dir` 回傳排序後的獨立路徑陣列，最多 10,000 項、路徑文字合計 1,000,000 位元組。受限 native Task 文件操作使用固定後台池；這不代表網絡呼叫已經使用同一通用等待機制。

## 參考如何維護

目前公開介面元資料位於 [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/stdlib/metadata.rs)，實作與規範一起用於核查。本網站暫時維護人工整理的核心參考；目前沒有已確認的 `ku doc` 命令。
