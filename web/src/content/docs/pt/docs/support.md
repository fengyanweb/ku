---
title: 'Suporte de plataformas e backends'
description: 'Diferencie plataformas publicadas do Ku, interpretador, executor, C nativo, subconjunto Task e suporte LLVM.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
---

Esta documentação descreve o escopo congelado da pré-versão experimental v0.0.18. A publicação efetiva e os downloads estão registrados no GitHub Releases.

## Plataformas CLI publicadas

A versão preliminar experimental v0.0.18 cobre Windows x64 (MSVC), Linux x64 (GNU) e macOS arm64 (Apple Silicon). Cada plataforma tem seu próprio arquivo. Não existe um único executável para os três sistemas. Não foram verificados arquivos de outras plataformas; consulte a [instalação](/pt/docs/install/).

## Caminhos de execução

| Capacidade                                                   | Interpretador `ku run`               | Executor padrão `ku build`                | C nativo síncrono                                       | Tasks nativos em desenvolvimento                  | Protótipo textual LLVM      |
| ------------------------------------------------------------ | ------------------------------------ | ----------------------------------------- | ------------------------------------------------------- | ------------------------------------------------- | --------------------------- |
| Linguagem básica, funções, controle                          | Implementados                        | Caminho do interpretador                  | Subconjunto implementado                                | Subconjunto tipado restrito                       | Protótipo limitado          |
| JSON                                                         | Suportado                            | Suportado                                 | Suportado                                               | Não suportado                                     | Não suportado               |
| Grafo completo de importações locais independente dos fontes | Carrega fontes                       | Não suportado; incorpora apenas a entrada | Expandido durante a geração                             | Expandido no subconjunto suportado                | Sem promessa de implantação |
| async / await                                                | Implementado com limites de recursos | Caminho do interpretador                  | Subconjunto específico ou rejeição                      | Parcial, sem async completo                       | Sem suporte completo        |
| Clientes PostgreSQL / Redis / MySQL                          | Sem acesso a bancos                  | Sem acesso interpretado a bancos          | Somente nativos                                         | Sem espera Task genérica                          | Não suportados              |
| Cliente TCP / TLS                                            | Sem caminho nativo correspondente    | Sem caminho nativo correspondente         | Implementação C gerada; TLS exige pacote correspondente | Sem afirmação de espera de rede genérica completa | Não suportado               |

As compilações padrão exigem a cadeia Rust correspondente e o vinculador. C nativo exige uma cadeia C para o destino. Passar em `ku check` não comprova que nenhuma das duas compilações funcione. Um comando LLVM não implica um backend de publicação completo.

## O subconjunto Task nativo

O ramo de desenvolvimento atual tem um caminho de código restrito para `async fn main(): null!`. Ele cobre as primitivas listadas explicitamente, strings, arrays planos, Results de camada única, início/movimentação/await de Task, parte do controle e operações limitadas de arquivos/diretórios em segundo plano. Não é um backend async completo no pacote publicado v0.0.18.

Continuam incompletos a espera de rede Task genérica, a execução M:N completa, as combinações arbitrárias de heap dinâmico, todas as combinações de capturas/funções async e os testes prolongados completos. Consulte os subconjuntos exatos aceitos e rejeitados no [contrato de concorrência](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md).

## Limites de bancos de dados e rede

As APIs de banco de dados usam `module.client(config)?`, seguido de métodos do receptor, com um pool limitado dentro do cliente. A validação em servidor real é separada para cada driver, sistema e commit. Uma ABI simulada ou uma vinculação bem-sucedida não substitui uma consulta real.

Redis usa TCP sem criptografia. MySQL não tem uma verificação uniforme de certificados que feche a conexão em caso de erro entre Oracle/MariaDB. Portanto, conexões diretas devem usar loopback, redes privadas controladas ou um túnel TLS verificado. PostgreSQL remoto exige `sslmode=verify-full` explícito e certificados raiz confiáveis. TLS nativo em `std.net` não significa que HTTP, Redis ou o interpretador reutilizem esse caminho.

## Auto-hospedagem e compatibilidade

O analisador léxico e o parser auto-hospedados do Ku têm subconjuntos limitados e verificações diferenciais. Um parser, verificador, IR e backend totalmente auto-hospedados continuam incompletos; Rust ainda fornece a cadeia de bootstrap. A versão 0.0.x não promete compatibilidade do código-fonte nem da ABI C/FFI gerada.

Os registros de trabalho e os resultados anteriores de CI comprovam somente o escopo e o commit declarados. Use as [notas de versão](/pt/releases/) para separar alterações publicadas do trabalho de desenvolvimento. Este site não informa como aprovados todos os testes CI do ramo atual, cargas de produção ou validações de bancos de dados.

> As atualizações de v0.0.18 abaixo estão por enquanto em inglês; o restante da tradução foi preservado. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions.

### Process memory boundary

| Host    | `ku supervise` guarantee and prerequisite                                                                                                                                                                                                                                                                   |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment.                                                          |
| macOS   | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge.                                                                                                           |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.
