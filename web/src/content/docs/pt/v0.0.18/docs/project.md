---
title: 'Projeto e contribuições'
description: 'Estado do projeto Ku, código público, limites de licença e relatos de problemas reproduzíveis.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
    'docs/v0.0.18-r1.md',
  ]
slug: pt/v0.0.18/docs/project
---

Ku é uma linguagem e uma cadeia de ferramentas: interpretador, runner executável padrão e compilação C nativa de um subconjunto compatível.

## Código-fonte e licença

Consulte o [código no GitHub](https://github.com/fengyanweb/ku) e a [página da versão](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). O código é visível publicamente, mas o código próprio do Ku **não tem concessão de licença de código aberto**. A extensão do editor está marcada como `UNLICENSED`. Publicar código ou binários não concede automaticamente uma licença de código aberto.

Os avisos de terceiros nos arquivos de versões aplicam-se somente aos respectivos componentes. Eles não estabelecem permissões para usar, modificar ou redistribuir o próprio Ku. Qualquer autorização necessária deve vir explicitamente do mantenedor; este site não cria novos compromissos de licença.

## Relate um problema

Use os [Issues](https://github.com/fengyanweb/ku/issues) do repositório para relatos reproduzíveis. Inclua:

- Versão do Ku, commit Git para fontes de desenvolvimento e sistema / arquitetura de destino.
- Comando completo, backend e código-fonte `.ku` mínimo.
- Diagnóstico completo ou Error estruturado, saída esperada e saída real.
- Versões das cadeias Rust / C para problemas de compilação e se há importações locais envolvidas.

Não envie tokens, senhas, strings de conexão reais ou logs com dados empresariais sensíveis. Relatos de desempenho devem incluir ambiente, carga de trabalho, método de medição e resultados brutos. Uma única amostra bem-sucedida não comprova vazão em produção.

## Antes de contribuir

Ao contribuir, apresente reprodução mínima, escopo e testes pertinentes. Código e documentação devem corresponder à mesma versão. Combine com o mantenedor a aceitação de contribuições e as permissões de uso, alteração e redistribuição de Ku.

O site não oferece execução de código Ku online.

Continue com os [primeiros passos](/pt/v0.0.18/docs/getting-started/), os [limites de suporte](/pt/v0.0.18/docs/support/) e as [notas de versão](/pt/v0.0.18/releases/).
