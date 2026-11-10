---
title: JSON 參考
description: Ku JSON 編碼、解碼、KuValue 轉換、錯誤標識和後端支援範圍。
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
slug: zh-tw/v0.0.18/docs/stdlib/json
---

`std.json` 提供 JSON 編解碼。`json.encode` 只讀編碼輸入，`json.decode` 產生獨立持有內容的 `KuValue`，兩者皆返回 Result。`stringify`、`parse` 與 `try_parse` 保留相容。

## 匯入與介面

```ku
import json from "std.json"
```

| 介面                         | 回傳       | 含義                               |
| ---------------------------- | ---------- | ---------------------------------- |
| `json.stringify(&value:any)` | `str!`     | 唯讀編碼，回傳新的精簡 JSON 文字   |
| `json.parse(text:str)`       | `KuValue!` | 嚴格解析，回傳獨立持有內容的動態值 |
| `json.try_parse(text:str)`   | `KuValue!` | `parse` 的兼容別名                 |
| `json.encode(&value:any)`    | `str!`     | 本版 `stringify` 的別名            |
| `json.decode(text:str)`      | `KuValue!` | 本版 `parse` 的別名                |

`!` 表示回傳 Result。用 `?` 傳播錯誤，或用 `try/catch` 處理；編碼不會消費或修改輸入。

## 編碼與讀取

以下為完整基本範例；更多用法見 [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.encode(value)?
    println(text)

    decoded = json.decode(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

物件鍵穩定排序，預期輸出為：

```text
{"count":7,"name":"Ku"}
Ku
8
```

解析回傳的 `KuValue` 不能直接參與整數算術。先呼叫 `.as_int()?` 或 `.as_str()?`；型別不匹配會回傳錯誤。缺少物件鍵也會回傳錯誤，不自動提供預設值。

## 資料與邊界

支援字串、布林、整數、有限浮點、null、巢狀陣列、字串鍵物件及可表示的 struct。函式、Task、資源控制代碼、Result、enum 與循環引用不能編碼。

| 規則        | 目前實作                                                       |
| ----------- | -------------------------------------------------------------- |
| JSON 語法   | 嚴格解析；拒絕注釋、尾逗號、額外尾部內容與非法 Unicode         |
| 輸入 / 輸出 | 各最多 1,000,000 位元組                                        |
| 深度預算    | 從根深度 0 計；`depth > 32` 拒絕，33 層空容器可接受，34 層拒絕 |
| 整數        | signed 64-bit；越界報錯，不自動轉 float                        |
| 浮點        | 有限 binary64；不保留原始數字拼寫、float 標籤或負零符號        |
| Unicode     | 支援中文、emoji、轉義及內嵌 NUL                                |
| 重復物件鍵  | 解析保留最後一個值                                             |
| 鍵順序      | 編碼按 UTF-8 位元組序穩定排序                                  |

`1.0` 可能編碼為 `1`。浮點值若會產生超出 int64 的整數文字，編碼器會補 `.0`，因此 `1e20` 編碼為 `100000000000000000000.0` 並能再次解碼。有限 binary64 不保留原始寫法、浮點標籤或負零符號；此修正已包含於本版。

## 錯誤

可恢復錯誤的 `domain` 是 `json`，常見 `code` 為 `parse_error`、`stringify_error`、`out_of_memory`。解析消息中的 `byte N` 是從 0 開始的 UTF-8 位元組位置。非法輸入可以在[錯誤與排查](/zh-tw/v0.0.18/docs/guide/errors/)頁面找到最小處理範例。

## 後端支援

解釋器、預設打包 runner 和同步 native C 已有 JSON 實作。**受限 native Task 子集與 LLVM 不支援 JSON。** 預設 runner 的 import 原始碼依賴仍存在，不能因 JSON 支援就宣稱完整獨立打包。

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md#127-json).
