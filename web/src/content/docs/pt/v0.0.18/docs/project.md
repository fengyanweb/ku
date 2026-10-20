---
title: Projeto e contribuições
description: Estado do projeto Ku, código público, limites de licença e relatos
  de problemas reproduzíveis.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - README.md
  - Cargo.toml
  - editors/vscode-ku/package.json
  - docs/v0.0.18.md
  - docs/syntax.md
slug: pt/v0.0.18/docs/project
---

Esta documentação descreve o escopo congelado da pré-versão experimental v0.0.18. A publicação efetiva e os downloads estão registrados no GitHub Releases. O projeto se chama **Ku programming language / Ku 编程语言**, com a CLI denominada `ku`. Ku é uma pequena linguagem de programação e interpretador em desenvolvimento. A versão pública atual é a preliminar experimental v0.0.18.

## Código-fonte e licença

Consulte o [código no GitHub](https://github.com/fengyanweb/ku) e a [página da versão](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). O código é visível publicamente, mas o código próprio do Ku **não tem concessão de licença de código aberto**. A extensão do editor está marcada como `UNLICENSED`. Publicar código ou binários não concede automaticamente uma licença de código aberto.

Os avisos de terceiros nos arquivos de versões aplicam-se somente aos respectivos componentes. Eles não estabelecem permissões para usar, modificar ou redistribuir o próprio Ku. Qualquer autorização necessária deve vir explicitamente do mantenedor; este site não cria novos compromissos de licença.

## Relate um problema

Use os [Issues](https://github.com/fengyanweb/ku/issues) do repositório para relatos reproduzíveis. Inclua:

* Versão do Ku, commit Git para fontes de desenvolvimento e sistema / arquitetura de destino.
* Comando completo, backend e código-fonte `.ku` mínimo.
* Diagnóstico completo ou Error estruturado, saída esperada e saída real.
* Versões das cadeias Rust / C para problemas de compilação e se há importações locais envolvidas.

Não envie tokens, senhas, strings de conexão reais ou logs com dados empresariais sensíveis. Relatos de desempenho devem incluir ambiente, carga de trabalho, método de medição e resultados brutos. Uma única amostra bem-sucedida não comprova vazão em produção.

## Antes de contribuir

O [contrato de sintaxe](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) e o [contrato semântico](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) definem o comportamento da linguagem. Verifique CI, implementação e contratos em conjunto. Uma meta no registro de trabalho não é um recurso implementado.

Mantenha as alterações focadas e explique reprodução, causa, alcance e validação. Revise o conteúdo do site junto ao código correspondente. Preserve testes existentes, alterações na árvore de trabalho e evidências únicas. O mantenedor determina a aceitação de contribuições e os acordos de licença; este site não inventa um acordo de contribuição.

Este site é estático. Seus textos, navegação e código não exigem backend empresarial. A busca carrega um índice Pagefind no navegador; o site não oferece execução remota de código Ku.

Continue com os [primeiros passos](/pt/v0.0.18/docs/getting-started/), os [limites de suporte](/pt/v0.0.18/docs/support/) e as [notas de versão](/pt/v0.0.18/releases/).
