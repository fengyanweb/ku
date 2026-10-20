---
title: 標準ライブラリ
description: Ku の標準ライブラリモジュール、主要なシグネチャ、バックエンドの制約。
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - src/stdlib/metadata.rs
  - src/stdlib/mod.rs
  - docs/syntax.md
  - docs/protocol-foundation.md
slug: ja/v0.0.18/docs/stdlib
---

本書は実験的プレリリース v0.0.18 の確定した範囲を説明します。実際の公開状況とダウンロードは GitHub Releases を確認してください。主要なエントリーポイントをソースと照合していますが、網羅的な API リファレンスではありません。動作は[構文仕様](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md)で定義されます。バックエンドの制約は[対応表](/ja/v0.0.18/docs/support/)をご覧ください。

## モジュールをインポートする

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`、`http`、`config`、`task`、`pg`、`redis`、`mysql`、`bytes`、`net` は明示的なインポートが必要です。一部の旧来のモジュールは直接呼び出せますが、新しい例では依存関係を明確にするため明示的にインポートします。`std.task` は内部観測と負荷試験用の名前空間であり、アプリケーションの並行処理 API ではありません。

| モジュール                           | 目的                                         | 注意すべき制約                                                                       |
| ------------------------------------ | -------------------------------------------- | ------------------------------------------------------------------------------------ |
| `std.string` / 文字列メソッド        | Unicode 長、バイト長、検索、切り出し         | ネイティブの `trim/lower/upper` には Unicode 実装がない                              |
| `std.array` / 配列メソッド           | 長さ、参照、新しい配列、map                  | 直接添字の失敗と、回復可能な `try_get` は異なる                                      |
| `std.object`                         | `get_or` による明示的な代替値付き参照        | 通常の添字でキーがなくても、既定で null にはならない                                 |
| [std.json](/ja/v0.0.18/docs/stdlib/json/)    | 厳密な JSON のエンコードとデコード           | ネイティブ Task と LLVM では未対応                                                   |
| `std.fs`                             | ファイル、存在確認、ディレクトリ一覧         | 相対パスは現在の `.ku` ソースのディレクトリを基準に解決する                          |
| `std.time`                           | 時刻、日付、期間、スリープ                   | 同期待機と Task 待機では実行経路が異なる                                             |
| `std.http`                           | HTTP クライアント、サービス、レスポンス      | バックエンドとライフサイクルの違いがある。汎用 Task ネットワークの完全な実装ではない |
| `std.config`                         | 環境と設定                                   | 実際の認証情報をソースにコミットしない                                               |
| `std.pg` / `std.redis` / `std.mysql` | 上限付きプールを備えた統一クライアント       | ネイティブ専用。実サーバー検証と TLS 制約の確認は別途必要                            |
| `std.bytes` / `std.net`              | バイナリデータ、TCP クライアント、任意の TLS | 生成ネイティブ C の経路。TLS にはターゲットに対応するパックが必要                    |
| `std.lexer` / `std.parser`           | コンパイラーのデバッグ補助                   | 完全な公開 Token / AST 構造ではなく、デバッグ概要を返す                              |

## 代表的なシグネチャ

以下は説明用シグネチャです。`&` は対応する読み取り専用パラメーターモード、`T!` は回復可能な結果を表します：

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

`"中😀".len()` は 2、`.byte_len()` は 7 です。長さは Unicode スカラー値を数えます。切り出しには UTF-8 バイトではなく文字インデックスを使います。`array.push` と `array.concat` は、入力を変更せず新しい配列を返します。

## ファイルを読む

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

先にソースと同じ場所に UTF-8 の `message.txt` を作成してください。ファイルがない場合や読み込み失敗時はエラーを返します。この例は、その入力ファイルに依存します。`try_read` と `try_write` は、同じ結果型を持つ互換エイリアスです。

`read_dir` はソート済みの独立した所有パスを返します。上限は 10,000 件、パス合計 1,000,000 バイトです。制限付きネイティブ Task のファイル操作は、固定バックグラウンドプールを使用します。同じ仕組みで汎用ネットワーク待機が実現していることを意味しません。

## リファレンスの保守

公開インターフェースのメタデータは [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/stdlib/metadata.rs) にあります。実装と仕様と併せて確認します。このウェブサイトでは現在、内容を選定した手動リファレンスを管理しています。既存の `ku doc` コマンドは確認されていません。
