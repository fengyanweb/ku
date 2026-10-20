---
title: 'はじめに'
description: 'Hello Ku を書き、ソースをチェックし、インタープリター実行とコンパイルの違いを理解します。'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources: ['examples/hello.ku', 'README.md', 'docs/syntax.md', 'docs/v0.0.18.md']
---

本書は実験的プレリリース v0.0.18 の確定した範囲を説明します。実際の公開状況とダウンロードは GitHub Releases を確認してください。このページでは、v0.0.17 に存在する基本構文と CLI コマンドを使用します。まず [Ku をインストール](/ja/docs/install/)して、`ku --version` が実行できることを確認してください。

## 最初のプログラムを書く

リポジトリの `examples/hello.ku` と同じ内容で `hello.ku` を作成します：

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` がエントリーポイントです。`println` はテキストを 1 行出力します。Ku では `let` や `let mut` を使用しません。

## チェックと実行

ファイルのディレクトリで実行します：

```sh
ku check hello.ku
ku run hello.ku
```

`check` はプログラムを実行せず、字句、構文、基本的な意味の誤りをチェックします。実行時に期待される標準出力：

```text
Hello Ku
```

Ku が `PATH` に含まれていない場合は、インストールした実行ファイルの絶対パスを指定してください。Windows の実行ファイル名は `ku.exe` です。

## 変数と関数

次の基本構文の断片は `main` 内に記述できます：

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

最初の代入で変数を宣言します。型を明示する場合は `name:type = value` を使用します。`print` は自動改行なしで出力し、`println` は改行を追加します。

## 実行ファイルをビルドする

必要なビルドツールはインストールのページに記載しています。既定のバックエンド：

```sh
ku build hello.ku
```

既定のバックエンドはインタープリターのランナーをパッケージ化します。ネイティブ C コンパイルには別途 C コンパイラーが必要です：

```sh
ku build --backend c hello.ku
```

既定の出力先は `.ku/build/<profile>/` 配下です。CLI が表示する正確なパスを使用してください。2 つのバックエンドは対応する機能が異なります。`ku check` の成功は、ネイティブビルドの成功を証明しません。

注意：`-o` なしの `ku build --native hello.ku` は、**C ソースだけを出力し、リンクは行いません**。最初のコンパイル済みプログラムには、上記の `--backend c` コマンドを使用してください。

## プロジェクトを作成する

パッケージディレクトリが必要な場合は、組み込みテンプレートを使用します：

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

パッケージのエントリーは `ku.mod` で設定します。`ku create --list` で利用可能なテンプレートを一覧表示できます。現在、`ku test` と `ku fmt` コマンドはありません。

続いて、[言語ガイド](/ja/docs/guide/language/)、[トラブルシューティング](/ja/docs/guide/errors/)、[バックエンドの対応範囲](/ja/docs/support/)をご覧ください。ここに示す出力は期待値の例であり、お使いのプラットフォームでのインストール検証に代わるものではありません。
