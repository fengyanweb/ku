---
title: 'Instalação e downloads'
description: 'Baixe a versão preliminar experimental Ku 0.0.18 e entenda os requisitos de plataforma e compilação.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-21'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

> As atualizações de v0.0.18 abaixo estão por enquanto em inglês; o restante da tradução foi preservado. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## Escolha a plataforma

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| Sistema | Arquitetura           | Arquivo                                                                                                                                               |
| ------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / arm64 | [`ku-v0.0.18-aarch64-apple-darwin.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18/ku-v0.0.18-aarch64-apple-darwin.tar.gz)         |
| Windows | x64 / MSVC            | [`ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18/ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz)     |
| Linux   | x64 / GNU             | [`ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.18/ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz) |

[Download checksums: SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.18/SHA256SUMS)

Não foram verificados arquivos para outras plataformas. O suporte à compilação cruzada não significa que todo sistema operacional ou arquitetura tenha um pacote publicado.

## Extraia e verifique

1. Verifique o arquivo compactado com `SHA256SUMS` da versão. Um `.manifest.json` separado descreve a integridade do arquivo e do seu conteúdo. As somas de verificação verificam a integridade; não são assinaturas do publicador.
2. Extraia **todo o arquivo compactado** em um diretório vazio e leia `README-INSTALL.md`. Preserve a organização relativa de `ku`, `libku.rlib`, `deps/` e `native-tls/`.
3. Confira a versão nesse diretório. No macOS / Linux:

```sh
./ku --version
```

No Windows PowerShell:

```powershell
.\ku.exe --version
```

A versão informada deve ser `0.0.18`. Opcionalmente, adicione este diretório a `PATH` e execute `ku --version`. Instale o arquivo incluído `ku-language-0.0.18.vsix` para integração com o VS Code.

## Interpreter and VS Code extension

Each platform archive above includes the Ku CLI (`ku run` uses the interpreter) and `ku-language-0.0.18.vsix`. The extension is distributed inside those archives, without a separate VSIX download asset. This website does not provide an in-browser interpreter.

After extraction, choose **… → Install from VSIX…** in the VS Code Extensions view, open `ku-language-0.0.18.vsix`, and reload the window. Alternatively, run this from the extracted directory:

```sh
code --install-extension ./ku-language-0.0.18.vsix
```

The extension also needs the Ku CLI. Add the extracted directory to `PATH`, or set `ku.executablePath` in VS Code to the full path of `ku` (`ku.exe` on Windows).

## Requisitos de execução e compilação

| Operação                                | Requisitos adicionais                                                                             |
| --------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | CLI do Ku para a plataforma correspondente                                                        |
| `ku build hello.ku` padrão              | Rust **1.89.0** para o destino e seu vinculador de sistema; manter as dependências Rust incluídas |
| `ku build --backend c hello.ku`         | Compilador e vinculador C funcionais para o destino                                               |
| Programa nativo de banco de dados       | Bibliotecas dinâmicas externas correspondentes do cliente de banco de dados; não estão incluídas  |

O executor padrão incorpora apenas o código-fonte de entrada. Os módulos importados continuam sendo carregados de seus locais originais. A implantação de um grafo completo de importações locais independente dos fontes exige compilação nativa em C e continua sujeita aos [limites de suporte dos backends](/pt/docs/support/).

## Experimente o código de desenvolvimento

A partir de uma cópia local do repositório:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Desinstale

Em uma instalação por arquivo compactado, remova o diretório extraído, desfaça as entradas `PATH` adicionadas e desinstale a extensão do VS Code. Primeiro preserve os arquivos-fonte e a configuração que criou nesse local. Não foram verificados um instalador separado de um clique, instalação por gerenciador de pacotes ou um comando `ku uninstall`.

Próximo passo: [execute o primeiro programa](/pt/docs/getting-started/). Fonte do contrato de instalação: [arquivos de versões](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/pt/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
