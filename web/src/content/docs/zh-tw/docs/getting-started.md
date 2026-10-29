---
title: 快速開始
description: 用 Ku 編寫 Hello Ku，檢查原始碼並瞭解解釋執行與構建的區別。
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'examples/hello.ku',
    'README.md',
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
---

先完成[安裝](/zh-tw/docs/install/)，並確認 `ku --version` 可執行。

## 寫下第一個程序

新建 `hello.ku`，內容與倉庫 `examples/hello.ku` 一致：

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` 是入口，`println` 輸出一行文字。Ku 原始碼不使用 `let` 或 `let mut`。

## 檢查並執行

在文件所在目錄執行：

```sh
ku check hello.ku
ku run hello.ku
```

`check` 檢查詞法、語法和基礎語義，不執行程序。正常執行的預期標準輸出為：

```text
Hello Ku
```

如果沒有把 Ku 加入 `PATH`，請使用安裝目錄里 `ku` 的完整路徑。Windows 可執行文件名為 `ku.exe`。

## 變數與函式

下面是基礎語法片段，放進 `main` 可以讀取並輸出變數：

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

首次賦值聲明變數；需要顯式型別時寫 `name:type = value`。`print` 不自動換行，`println` 會換行。

## 構建可執行文件

安裝頁列出了構建工具前置要求。預設構建：

```sh
ku build hello.ku
```

預設後端生成解釋器打包型可執行文件。native C 後端需要單獨的 C 編譯器：

```sh
ku build --backend c hello.ku
```

預設輸出位於 `.ku/build/<profile>/`；以實際 CLI 輸出路徑為準。兩種構建方式的覆蓋範圍不同，不能把 `ku check` 成功當作 native 編譯成功。

注意：`ku build --native hello.ku` **不帶 `-o` 時只生成 C 原始碼**，不執行鏈接。首個專案建議使用上面的 `--backend c` 形式。

## 創建專案

需要包目錄時，可使用內置模板：

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

包通過 `ku.mod` 指定入口。`ku create --list` 查看真實模板列表；目前沒有 `ku test` 或 `ku fmt` 命令。

繼續閱讀[語言指南](/zh-tw/docs/guide/language/)、[錯誤與排查](/zh-tw/docs/guide/errors/)和[後端支援](/zh-tw/docs/support/)。
