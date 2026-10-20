---
title: 'JSON リファレンス'
description: 'Ku の JSON エンコード、デコード、KuValue 変換、エラーコード、バックエンド対応。'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
  ]
---

> v0.0.18 の追加情報は現在英語で掲載しています。既存の翻訳は保持しています。 [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## インポートとインターフェース

```ku
import json from "std.json"
```

| インターフェース             | 戻り値     | 意味                                                       |
| ---------------------------- | ---------- | ---------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | 読み取り専用で新しいコンパクトな JSON テキストにエンコード |
| `json.parse(text:str)`       | `KuValue!` | 独立した所有の動的な値へ厳密に解析                         |
| `json.try_parse(text:str)`   | `KuValue!` | `parse` の互換エイリアス                                   |
| `json.encode(&value:any)`    | `str!`     | 開発ブランチの `stringify` エイリアス                      |
| `json.decode(text:str)`      | `KuValue!` | 開発ブランチの `parse` エイリアス                          |

`!` は Result を表します。`?` でエラーを伝播するか、`try/catch` で処理します。エンコードは入力を消費せず、変更もしません。

## 値をエンコードして読む

次は完全な基本例です。開発ブランチの [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18/examples/json.ku) には、より広範な回帰検証例があります。

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

オブジェクトのキーは決定的な順序でソートされます。期待される出力：

```text
{"count":7,"name":"Ku"}
Ku
8
```

解析済みの `KuValue` を直接整数演算に使うことはできません。まず `.as_int()?` または `.as_str()?` で変換してください。型が一致しない場合はエラーを返します。存在しないオブジェクトキーも、自動の既定値ではなくエラーを返します。

## データと境界

対応する入力は、文字列、真偽値、整数、有限の浮動小数点数、null、入れ子の配列、文字列キーのオブジェクト、表現可能な構造体です。関数、Task、リソースハンドル、Result、列挙型、循環参照はエンコードできません。

| 規則                     | 現在の実装                                                                         |
| ------------------------ | ---------------------------------------------------------------------------------- |
| JSON 構文                | 厳密。コメント、末尾のカンマ、余分な末尾入力、不正な Unicode を拒否                |
| 入出力                   | それぞれ 1,000,000 バイトまで                                                      |
| 深さの予算               | ルートの深さは 0。`depth > 32` を拒否。空コンテナー 33 階層は受理し、34 階層は拒否 |
| 整数                     | 符号付き 64 ビット。オーバーフローはエラーとなり、浮動小数点数に自動変換しない     |
| 浮動小数点数             | 有限の binary64。元の表記、float のタグ、負のゼロは保持しない                      |
| Unicode                  | 中国語、絵文字、エスケープ、埋め込まれた NUL                                       |
| 重複するオブジェクトキー | 解析時は最後の値を保持                                                             |
| キーの順序               | エンコード時は UTF-8 バイト順にソート                                              |

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## エラー

回復可能なエラーは `json` ドメインを使用します。主なコードは `parse_error`、`stringify_error`、`out_of_memory` です。解析メッセージの `byte N` は 0 始まりの UTF-8 バイトオフセットです。不正入力を処理する最小例は[エラーとトラブルシューティング](/ja/docs/guide/errors/)を参照してください。

## バックエンド対応

インタープリター、既定のパッケージ化ランナー、同期ネイティブ C バックエンドには JSON 実装があります。**制限付きネイティブ Task サブセットと LLVM は JSON に対応していません。** 既定のランナーでは、インポートしたソースの元の場所への依存が残ります。JSON 対応は、完全に独立したパッケージを意味しません。

このページのシグネチャと制約は、ソースおよび仕様と照合しました。実行検証ではバージョン、コミット、バックエンドを明記する必要があります。完全な [JSON 構文仕様](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md#127-json)をご覧ください。
