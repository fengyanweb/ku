---
title: 'Projeto e contribuições'
description: 'Estado do projeto Ku, código público, limites de licença e relatos de problemas reproduzíveis.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

A documentação atual corresponde ao Ku 0.0.19, uma pré-versão experimental com a mesma versão da CLI e da extensão VS Code. A disponibilidade, os arquivos e as somas de verificação dependem do GitHub Release efetivo.

**Ku programming language / Ku 编程语言** · CLI: `ku`.

## Código-fonte e licença

[GitHub](https://github.com/fengyanweb/ku) · [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)

Ku e a extensão VS Code usam a Apache License 2.0. Consulte [LICENSE](https://github.com/fengyanweb/ku/blob/main/LICENSE) e [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE). Os componentes de terceiros mantêm seus próprios direitos autorais e licenças.

## Relate um problema

Use os [Issues](https://github.com/fengyanweb/ku/issues) do repositório para relatos reproduzíveis. Inclua:

Feedback por e-mail: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Versão do Ku, commit Git para fontes de desenvolvimento e sistema / arquitetura de destino.
- Comando completo, backend e código-fonte `.ku` mínimo.
- Diagnóstico completo ou Error estruturado, saída esperada e saída real.
- Versões das cadeias Rust / C para problemas de compilação e se há importações locais envolvidas.

Não envie tokens, senhas, strings de conexão reais ou logs com dados empresariais sensíveis. Relatos de desempenho devem incluir ambiente, carga de trabalho, método de medição e resultados brutos. Uma única amostra bem-sucedida não comprova vazão em produção.

## Antes de contribuir

O [contrato de sintaxe](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) e o [contrato semântico](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) definem o comportamento da linguagem. Verifique CI, implementação e contratos em conjunto. Uma meta no registro de trabalho não é um recurso implementado.

Mantenha as alterações focadas e explique reprodução, causa, alcance e validação. Revise o conteúdo do site junto ao código correspondente. Preserve testes existentes, alterações na árvore de trabalho e evidências únicas. O mantenedor determina a aceitação de contribuições e os acordos de licença; este site não inventa um acordo de contribuição.

Este site é estático. Seus textos, navegação e código não exigem backend empresarial. A busca carrega um índice Pagefind no navegador; o site não oferece execução remota de código Ku.

Continue com os [primeiros passos](/pt/docs/getting-started/), os [limites de suporte](/pt/docs/support/) e as [notas de versão](/pt/releases/).
