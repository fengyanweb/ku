---
title: 'Supporto di piattaforme e backend'
description: 'Distingui piattaforme pubblicate di Ku, interprete, runner, C nativo, sottoinsieme Task e supporto LLVM.'
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

Questa documentazione descrive l’ambito fissato della versione preliminare sperimentale v0.0.18. La pubblicazione effettiva e i download sono registrati in GitHub Releases.

## Piattaforme CLI pubblicate

La versione preliminare sperimentale v0.0.18 copre Windows x64 (MSVC), Linux x64 (GNU) e macOS arm64 (Apple Silicon). Ogni piattaforma ha un archivio proprio. Non esiste un unico eseguibile per tutti e tre i sistemi. Non sono stati verificati file per altre piattaforme; consulta l'[installazione](/it/docs/install/).

## Percorsi di esecuzione

| Capacità                                                        | Interprete `ku run`                   | Runner predefinito `ku build`             | C nativo sincrono                                                    | Task nativi in sviluppo                                  | Prototipo testuale LLVM           |
| --------------------------------------------------------------- | ------------------------------------- | ----------------------------------------- | -------------------------------------------------------------------- | -------------------------------------------------------- | --------------------------------- |
| Linguaggio di base, funzioni, controllo                         | Implementati                          | Percorso interprete                       | Sottoinsieme implementato                                            | Sottoinsieme tipizzato ristretto                         | Prototipo limitato                |
| JSON                                                            | Supportato                            | Supportato                                | Supportato                                                           | Non supportato                                           | Non supportato                    |
| Grafo completo di importazioni locali indipendente dal sorgente | Carica sorgenti                       | Non supportato; incorpora solo l'ingresso | Espanso durante la generazione                                       | Espanso nel sottoinsieme supportato                      | Nessuna promessa di distribuzione |
| async / await                                                   | Implementato con limiti di risorse    | Percorso interprete                       | Sottoinsieme dedicato o rifiuto                                      | Parziale, async non completo                             | Nessun supporto completo          |
| Client PostgreSQL / Redis / MySQL                               | Nessun accesso al database            | Nessun accesso interpretato al database   | Solo nativi                                                          | Nessuna attesa Task generica                             | Non supportati                    |
| Client TCP / TLS                                                | Nessun percorso nativo corrispondente | Nessun percorso nativo corrispondente     | Implementazione C generata; TLS richiede un pacchetto corrispondente | Nessuna affermazione di attesa di rete generica completa | Non supportato                    |

Le compilazioni predefinite richiedono la toolchain Rust corrispondente e il linker. C nativo richiede una toolchain C per la destinazione. Superare `ku check` non dimostra che uno dei due processi di compilazione funzioni. Un comando LLVM non implica un backend di rilascio completo.

## Il sottoinsieme Task nativo

Il ramo di sviluppo attuale ha un percorso sorgente ristretto per `async fn main(): null!`. Copre le primitive elencate esplicitamente, stringhe, array piatti, Results a un solo livello, avvio/spostamento/await dei Task, parte del controllo e operazioni limitate di file/directory in background. Non è un backend async completo nel pacchetto pubblicato v0.0.18.

Restano incompleti attese di rete Task generiche, esecuzione M:N completa, combinazioni arbitrarie di heap dinamico, tutte le combinazioni di catture/funzioni async e test prolungati completi. Consulta il sottoinsieme esatto accettato e rifiutato nel [contratto di concorrenza](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md).

## Limiti di database e rete

Le API di database usano `module.client(config)?`, seguito dai metodi del ricevitore, con un pool limitato nel client. La validazione con server reali è separata per ogni driver, sistema e commit. Un'ABI simulata o un collegamento riuscito non sostituiscono una query reale.

Redis usa TCP in chiaro. MySQL non dispone di una verifica coerente dei certificati con chiusura in caso di errore fra Oracle/MariaDB. Le connessioni dirette dovrebbero quindi usare loopback, reti private controllate o un tunnel TLS verificato. PostgreSQL remoto richiede `sslmode=verify-full` esplicito e certificati radice attendibili. Il TLS nativo in `std.net` non significa che HTTP, Redis o l'interprete riutilizzino quel percorso.

## Self-hosting e compatibilità

Il lexer e il parser self-hosted di Ku hanno sottoinsiemi delimitati e controlli differenziali. Un parser, checker, IR e backend completamente self-hosted restano incompleti; Rust fornisce ancora la toolchain di bootstrap. La versione 0.0.x non promette compatibilità del sorgente o dell'ABI C/FFI generata.

I registri di lavoro e i risultati CI precedenti dimostrano solo ambito e commit dichiarati. Usa le [note di versione](/it/releases/) per separare i cambiamenti pubblicati dal lavoro di sviluppo. Questo sito non riporta come superati tutti i controlli CI del ramo attuale, i carichi di produzione o le accettazioni del database.

> Gli aggiornamenti v0.0.18 seguono per ora in inglese; il resto della traduzione è mantenuto. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

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
