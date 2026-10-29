---
title: 'Projeto e contribuições'
description: 'Estado do projeto Ku, código público, limites de licença e relatos de problemas reproduzíveis.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'LICENSE',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Ku é uma linguagem e uma cadeia de ferramentas: interpretador, runner executável padrão e compilação C nativa de um subconjunto compatível.

## Código-fonte e licença

Ku e a extensão para VS Code são distribuídos sob **Apache-2.0**. Consulte o [código-fonte](https://github.com/fengyanweb/ku), a [versão 0.0.19](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) e o [texto da licença](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE).

Os componentes de terceiros mantêm suas próprias licenças. Os pacotes incluem os textos e avisos correspondentes.

## Relate um problema

Use os [Issues](https://github.com/fengyanweb/ku/issues) do repositório para relatos reproduzíveis. Inclua:

Feedback por e-mail: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Versão do Ku, commit Git para fontes de desenvolvimento e sistema / arquitetura de destino.
- Comando completo, backend e código-fonte `.ku` mínimo.
- Diagnóstico completo ou Error estruturado, saída esperada e saída real.
- Versões das cadeias Rust / C para problemas de compilação e se há importações locais envolvidas.

Não envie tokens, senhas, strings de conexão reais ou logs com dados empresariais sensíveis. Relatos de desempenho devem incluir ambiente, carga de trabalho, método de medição e resultados brutos. Uma única amostra bem-sucedida não comprova vazão em produção.

## Antes de contribuir

Mantenha as alterações focadas e inclua uma reprodução mínima, o escopo e as verificações pertinentes. Código, documentação e exemplos devem corresponder à mesma versão. Os responsáveis revisam as contribuições antes de integrá-las.

O site não oferece execução de código Ku online.

Continue com os [primeiros passos](/pt/docs/getting-started/), os [limites de suporte](/pt/docs/support/) e as [notas de versão](/pt/releases/).
