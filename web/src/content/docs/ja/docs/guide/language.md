---
title: '言語ガイド'
description: 'Ku の変数、関数、制御フロー、所有権、モジュール、回復可能なエラーを学びます。'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
    'docs/v0.0.18-r1.md',
  ]
---

このガイドでは中心となる構文を紹介します。リポジトリの[構文仕様](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md)と[意味仕様](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md)が正式な定義です。以下は学習用の断片です。差異については[バックエンドの対応範囲](/ja/docs/support/)を参照してください。

## 変数と型

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

最初の代入で変数を宣言します。プリミティブ型は `int`、`float`、`bool`、`str`、`null` です。整数は符号付き 64 ビット、浮動小数点数は binary64 です。整数を真偽値に暗黙変換して条件にすることはできません。

## 関数と制御フロー

次の完全な基本例は `examples/function.ku` にあります：

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

期待される出力は `30` です。次の制御フローの断片は `main` 内に記述できます：

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` は `0` から `4` まで反復します。`if` と `while` の条件には `bool` が必要です。複数の文には波括弧を使います。分岐式は `match` を使用し、`switch` はありません。

<a id="ownership"></a>

## 所有権と読み取り専用の借用

Owned 値は原則 move され、所有スコープ終了時に決定的に drop されます。独立した複製には `.clone()` が必要です。グローバルなトレーシング GC は導入しませんが、クロージャ環境などの内部共有構造は参照カウントを使う場合があり、実行時の管理コストは残ります。同期の読み取り専用借用は `&name: T` の宣言で決まります。通常の Owned パラメーターは読むだけでも値を move し得ます。呼び出しは `length(value)` のままで、`&value` ではありません。async 借用は非対応です。

`int`、`float`、`bool`、`null` は Copy 値です。文字列、配列、オブジェクト、構造体、列挙型、Result、関数値は Owned です。代入と通常の引数は、既定で所有値をムーブします。独立した値が必要な場合は `.clone()` を使用してください。Task はクローンできません。

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

同期関数では、読み取り専用の借用パラメーターを宣言できます：

```ku
fn length(&text: str): int {
    return text.len()
}
```

引数に `&` を付けず、`length(value)` として呼び出します。借用データの変更、ムーブによる取り出し、呼び出し外への持ち出しはできません。非同期パラメーターの借用などの組み合わせには、まだ対応していません。読み取り専用の借用は、任意の関数の純粋性を保証するものではありません。

## 構造体、オブジェクト、モジュール

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

動的オブジェクトは `{ name: "Ku" }` で表します。通常のオブジェクト添字でキーが存在しない場合は失敗します。`value["name"]?` は Result を通して参照を処理し、成功時に `KuValue` を返します。具体的な型を得るには `.as_str()?` や `.as_int()?` を使用してください。存在しないキーが暗黙に `null` になることはありません。

標準ライブラリのインポートには `import json from "std.json"` のようなドット区切りのパスを使います。ローカルモジュールでは `import math from "./math"` が使えます。ユーザー定義のトップレベルエクスポートは ASCII 大文字で始めます。小文字の名前はファイル内に限定されます。

## 回復可能なエラー

戻り値の型 `T!` は、T または Error を返します。`?` は回復可能なエラーを伝播し、`fail` はエラーを作成します：

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

Error には `domain`、`code`、`message` フィールドがあります。通常の `catch` は `panic`、Task のキャンセル、タイムアウトを捕捉しません。finally 内のキャンセル時クリーンアップも、共通の絶対期限の予算を使用します。任意のユーザークリーンアップの完了は保証されません。

## 並行処理の境界

アプリケーションコードは `async fn` を呼び出して一度だけ使用できる Task ハンドルを取得し、`await` を使用します。`task.spawn`、`Task.new`、`thread.spawn` というアプリケーション API はありません。インタープリターとネイティブ Task サブセットの対応範囲は異なります。あるバックエンドで動作する例は、別のバックエンドでの完全な対応を証明しません。[プラットフォームとバックエンドの対応範囲](/ja/docs/support/)を参照してください。
