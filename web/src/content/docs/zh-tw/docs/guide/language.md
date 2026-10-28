---
title: 語言指南
description: 學習 Ku 的變數、函式、控制流、所有權、模組與可恢復錯誤。
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
  ]
---

本文檔對應 v0.0.18 實驗預發行版的凍結範圍；實際公開狀態與下載資產以 GitHub Release 為準。這裡介紹核心寫法，完整規則以倉庫[語法合同](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md)和[語義合同](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md)為準。下列為教學片段，後端差異見[支援矩陣](/zh-tw/docs/support/)。

## 變數與型別

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

首次賦值即聲明變數；基本型別為 `int`、`float`、`bool`、`str`、`null`。整數為有符號 64 位，浮點使用 binary64。不會通過 truthy 轉換把整數當條件。

## 函式與控制流

完整基礎函式範例來自 `examples/function.ku`：

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

預期輸出 `30`。下列控制流片段可放入 `main`：

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` 遍歷 `0` 到 `4`。`if` 與 `while` 條件必須是 `bool`；多語句 body 使用花括號。分支表達式使用 `match`，沒有 `switch`。

## 所有權與唯讀借用

`int`、`float`、`bool`、`null` 是 Copy 值。字串、陣列、物件、結構體、enum、Result 與函式值按 Owned 處理，賦值和普通傳參預設 move；需要保留獨立值時顯式 `.clone()`。Task 不可 clone。

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

同步函式可以聲明唯讀借用參數：

```ku
fn length(&text: str): int {
    return text.len()
}
```

呼叫仍寫 `length(value)`，不在實參前加 `&`。借用不能修改來源、移出其 owned 內容或逃逸呼叫作用域；async borrowed 參數等組合尚不支援。唯讀借用不是任意函式純度保證。

## 結構體、物件與模組

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

動態物件使用 `{ name: "Ku" }`。普通物件索引缺鍵會失敗；`value["name"]?` 以 Result 處理缺鍵，成功值為 `KuValue`，需要具體型別時用 `.as_str()?` 或 `.as_int()?`。缺鍵不會自動變成 `null`。

標準函式庫使用點分路徑，例如 `import json from "std.json"`；本地模組可寫 `import math from "./math"`。用戶頂層匯出名以 ASCII 大寫字母開頭，小寫名字保留在文件內部。

## 可恢復錯誤

回傳型別 `T!` 表示可能成功回傳 T，也可能回傳 Error。`?` 傳播可恢復錯誤，`fail` 產生錯誤：

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    try {
        println(read_name()?)
    } catch (error) {
        println(error.message)
    }
}
```

Error 包含 `domain`、`code`、`message`。`panic`、Task 取消和超時不由普通 `catch` 捕獲。取消時的 finally 清理有共享的絕對截止預算，不保證執行完任意用戶清理邏輯。

## 併發的目前邊界

普通開發者通過 `async fn` 獲得一次性 Task 控制代碼，再 `await`；沒有 `task.spawn`、`Task.new` 或 `thread.spawn` 業務入口。解釋器和 native Task 子集的覆蓋不同，不應從一種後端的範例推導另一種後端完整支援。參見[平台與後端支援](/zh-tw/docs/support/)。
