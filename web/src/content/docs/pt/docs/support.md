---
title: 'Suporte de plataformas e backends'
description: 'Diferencie plataformas publicadas do Ku, interpretador, executor, C nativo, subconjunto Task e suporte LLVM.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Baixar o Ku 0.0.19

O Ku 0.0.19 é uma pré-versão experimental. Estão disponíveis pacotes para macOS ARM64 (Apple Silicon), Windows x64 e Linux x64 (GNU).

[Pacotes e instruções de instalação](/pt/docs/install/)

## Modos de execução

- **`ku run`**: Interpretador da linguagem, com JSON geral e handlers HTTP síncronos; sem clientes de banco de dados.
- **`ku build`**: O runner padrão incorpora o arquivo de entrada; imports locais ainda precisam dos arquivos-fonte. Exige Rust **1.89.0** e um vinculador do sistema.
- **`ku build --backend c`**: Oferece JSON síncrono, HTTP com `http.json` e clientes nativos de PostgreSQL, Redis e MySQL. Exige compilador C, vinculador e bibliotecas cliente compatíveis com o destino. Operações de banco de dados não são repetidas automaticamente.
- **Task**: O caminho Task nativo oferece um subconjunto tipado com início/`await`, timers, algumas operações de E/S, handlers HTTP async nomeados e determinadas consultas PG. Nesse caminho, JSON só codifica objetos literais com valores escalares; não oferece decodificação geral. O M:N completo e a cobertura geral de E/S ainda não estão concluídos.
- **LLVM**: O backend de texto LLVM continua sendo um protótipo limitado.

## Problemas conhecidos

As verificações de publicação e de pacotes passaram nas três plataformas. Isso não é uma aprovação geral para produção.

- **HTTP 504:** ocorreu no passado, mas não foi reproduzido na revisão curta mais recente; a causa ainda não foi confirmada.
- **Picos de latência durante o cancelamento:** na última revisão de 30 segundos, todas as 60.000 requisições curtas estavam corretas. O p99 foi **1.557 ms** e o máximo **98.809 ms**; algumas requisições ainda ultrapassaram 20 ms.

A validação formal de estabilidade de 30 minutos ainda não passou. Medições curtas não a substituem.

Referência completa: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

Relatar problemas: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
