---
title: 'Instalação e downloads'
description: 'Baixe o Ku 0.0.20 para seu computador. Cada pacote inclui a ferramenta de linha de comando, o interpretador e a extensão do VS Code.'
kuVersion: '0.0.20'
verifiedAt: '2026-11-09'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.20.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Baixe o **Ku 0.0.20** para seu computador. Cada pacote inclui a ferramenta de linha de comando, o interpretador e a extensão do VS Code.

## 1. Baixar o Ku

| Sistema | Arquitetura           | Baixar                                                                                                                          |
| ------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / ARM64 | [Baixar macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-aarch64-apple-darwin.tar.gz)     |
| Windows | x64                   | [Baixar Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-pc-windows-msvc.tar.gz) |
| Linux   | x64 / GNU             | [Baixar Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-unknown-linux-gnu.tar.gz) |

## 2. Extrair e executar

Extraia o **pacote completo** para uma pasta vazia, como `ku` na sua pasta de usuário. Mantenha todos os arquivos juntos; não mova apenas o executável. Abra um terminal nessa pasta.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

A saída deve ser `ku 0.0.20`. Você já pode executar e verificar arquivos `.ku`; essas operações não exigem a instalação do Rust.

<details>
<summary>Opcional: usar ku em qualquer pasta</summary>

Se você extraiu para `~/ku`, adicione esta linha ao arquivo de inicialização do seu shell, como `~/.zshrc` ou `~/.bashrc`:

```sh
export PATH="$HOME/ku:$PATH"
```

No Windows, abra «Editar as variáveis de ambiente da sua conta», edite o `Path` do usuário e adicione o caminho completo da pasta extraída, por exemplo `%USERPROFILE%\ku`. Mantenha as entradas existentes.

Abra um novo terminal e execute `ku --version`.

</details>

## 3. Instalar a extensão do VS Code

Na visualização Extensões do VS Code, selecione **… → Instalar do VSIX…**, abra `ku-language-0.0.20.vsix` na pasta extraída e recarregue a janela.

Se o Ku não estiver no `PATH`, defina `ku.executablePath` como o caminho completo de `ku` (`ku.exe` no Windows).

<details>
<summary>Verificar o download</summary>

Compare o SHA-256 do pacote com [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.20/SHA256SUMS). A [página da versão](https://github.com/fengyanweb/ku/releases/tag/v0.0.20) também oferece um `.manifest.json` por pacote com as somas de verificação dos arquivos. Veja outras instruções em `README-INSTALL.md` dentro do pacote.

</details>

<details>
<summary>Compilar seus próprios programas Ku</summary>

`ku run` e `ku check` não exigem Rust. O `ku build` padrão exige um vinculador do sistema e a identidade exata do compilador Rust registrada no SDK instalado; os pacotes v0.0.20 usam **Rust 1.99.1**. `ku build --backend c` exige um compilador C e um vinculador para o destino. Programas nativos de banco de dados também precisam das bibliotecas cliente correspondentes, não incluídas no pacote.

</details>

<details>
<summary>Desinstalar</summary>

Preserve os arquivos-fonte e as configurações que criou. Depois remova a pasta extraída, a entrada adicionada ao `PATH` e a extensão do VS Code.

</details>

[Próximo passo: executar seu primeiro programa](/pt/docs/getting-started/)
