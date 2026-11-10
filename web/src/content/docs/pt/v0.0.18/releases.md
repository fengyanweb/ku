---
title: 'Notas de versão'
description: 'Ku 0.0.18 — Notas de versão'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
slug: pt/v0.0.18/releases
---

## v0.0.18

Corrige avaliação repetida de receptores em cadeias de chamadas, constantes float do C nativo e `time.date(Time): Date!`, além de atualizar a extensão VS Code.

- JSON: `encode`/`decode` compartilham `stringify`/`parse`, incluindo a correção de ida e volta de `1e20`.

[Baixar Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Suporte de plataformas e backends](/pt/v0.0.18/docs/support/)

## v0.0.17 · Versão preliminar experimental

Principais alterações:

- Parâmetros emprestados síncronos somente para leitura usam `&name: T`, com argumentos de chamada comuns. Parâmetros normais com propriedade mantêm a semântica de movimentação.
- O subconjunto do parser auto-hospedado adiciona `while` com chaves, com limites de aninhamento e quantidade de instruções.
- Os arquivos incluem dependências Rust correspondentes para o executor padrão e um pacote TLS para o destino. Cadeias de ferramentas externas adequadas continuam necessárias.

Os downloads cobrem Windows x64, Linux x64 e macOS arm64. Os sete arquivos da versão são três arquivos de plataforma, três manifestos e `SHA256SUMS`. Consulte a [instalação](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) e as [notas completas da versão](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Compatibilidade e limites

O Ku continua experimental e não está pronto para produção. A versão 0.0.x não promete compatibilidade de código-fonte nem da ABI C/FFI gerada. Recompile os artefatos nativos correspondentes ao atualizar. Parâmetros async emprestados, auto-hospedagem completa e async nativo completo estão fora desta versão.

O executor padrão incorpora apenas o código-fonte de entrada; módulos importados ainda exigem suas localizações originais. Os clientes de bancos de dados são somente nativos. Verifique os limites de servidor real, TLS e estado de sessão de cada driver. O código próprio do Ku continua sem concessão de licença de código aberto.
