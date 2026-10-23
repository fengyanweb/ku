---
title: 'Projeto e contribuições'
description: 'Estado do projeto Ku, código público, limites de licença e relatos de problemas reproduzíveis.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

Esta documentação descreve o escopo congelado da pré-versão experimental v0.0.18. A publicação efetiva e os downloads estão registrados no GitHub Releases. O projeto se chama **Ku programming language / Ku 编程语言**, com a CLI denominada `ku`. Ku é uma pequena linguagem de programação e interpretador em desenvolvimento. A versão pública atual é a preliminar experimental v0.0.18.

## Código-fonte e licença

O código próprio do Ku nos instantâneos de fontes que incluem o arquivo de licença, assim como a extensão atual do editor, são distribuídos sob [Apache License 2.0](https://github.com/fengyanweb/ku/blob/main/LICENSE) (`Apache-2.0`); consulte [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE) para as atribuições. Consulte o [código no GitHub](https://github.com/fengyanweb/ku) e a [página da versão](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Os componentes de terceiros mantêm suas próprias licenças.

Os arquivos de v0.0.18-r1 e de versões anteriores mantêm os materiais de licença originais que os acompanham; esta mudança de licença dos fontes não os reempacota. Usar o compilador Ku não exige publicar o código-fonte dos seus próprios programas. Ao distribuir programas que contêm código do runtime do Ku, cumpra os requisitos de licença e atribuição aplicáveis, incluindo os avisos de terceiros.

## Relate um problema

Use os [Issues](https://github.com/fengyanweb/ku/issues) do repositório para relatos reproduzíveis. Inclua:

- Versão do Ku, commit Git para fontes de desenvolvimento e sistema / arquitetura de destino.
- Comando completo, backend e código-fonte `.ku` mínimo.
- Diagnóstico completo ou Error estruturado, saída esperada e saída real.
- Versões das cadeias Rust / C para problemas de compilação e se há importações locais envolvidas.

Não envie tokens, senhas, strings de conexão reais ou logs com dados empresariais sensíveis. Relatos de desempenho devem incluir ambiente, carga de trabalho, método de medição e resultados brutos. Uma única amostra bem-sucedida não comprova vazão em produção.

## Antes de contribuir

O [contrato de sintaxe](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) e o [contrato semântico](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) definem o comportamento da linguagem. Verifique CI, implementação e contratos em conjunto. Uma meta no registro de trabalho não é um recurso implementado.

Mantenha as alterações focadas e explique reprodução, causa, alcance e validação. Revise o conteúdo do site junto ao código correspondente. Preserve testes existentes, alterações na árvore de trabalho e evidências únicas. O mantenedor determina a aceitação de contribuições e os acordos de licença; este site não inventa um acordo de contribuição.

Este site é estático. Seus textos, navegação e código não exigem backend empresarial. A busca carrega um índice Pagefind no navegador; o site não oferece execução remota de código Ku.

Continue com os [primeiros passos](/pt/docs/getting-started/), os [limites de suporte](/pt/docs/support/) e as [notas de versão](/pt/releases/).
