---
title: 'Supporto di piattaforme e backend'
description: 'Distingui piattaforme pubblicate di Ku, interprete, runner, C nativo, sottoinsieme Task e supporto LLVM.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
    'docs/v0.0.18-r1.md',
  ]
slug: it/v0.0.18/docs/support
---

## Piattaforme CLI pubblicate

La versione preliminare sperimentale v0.0.18 copre Windows x64 (MSVC), Linux x64 (GNU) e macOS arm64 (Apple Silicon). Ogni piattaforma ha un archivio proprio. Non esiste un unico eseguibile per tutti e tre i sistemi. Non sono stati verificati file per altre piattaforme; consulta l'[installazione](/it/v0.0.18/docs/install/).

## Percorsi di esecuzione

| Capacità                                                        | Interprete `ku run`                   | Runner predefinito `ku build`             | C nativo sincrono                                                    | Task nativo (sottoinsieme)                               | Prototipo testuale LLVM           |
| --------------------------------------------------------------- | ------------------------------------- | ----------------------------------------- | -------------------------------------------------------------------- | -------------------------------------------------------- | --------------------------------- |
| Linguaggio di base, funzioni, controllo                         | Implementati                          | Percorso interprete                       | Sottoinsieme implementato                                            | Sottoinsieme tipizzato ristretto                         | Prototipo limitato                |
| JSON                                                            | Supportato                            | Supportato                                | Supportato                                                           | Non supportato                                           | Non supportato                    |
| Grafo completo di importazioni locali indipendente dal sorgente | Carica sorgenti                       | Non supportato; incorpora solo l'ingresso | Espanso durante la generazione                                       | Espanso nel sottoinsieme supportato                      | Nessuna promessa di distribuzione |
| async / await                                                   | Implementato con limiti di risorse    | Percorso interprete                       | Sottoinsieme dedicato o rifiuto                                      | Parziale, async non completo                             | Nessun supporto completo          |
| Client PostgreSQL / Redis / MySQL                               | Nessun accesso al database            | Nessun accesso interpretato al database   | Solo nativi                                                          | Nessuna attesa Task generica                             | Non supportati                    |
| Client TCP / TLS                                                | Nessun percorso nativo corrispondente | Nessun percorso nativo corrispondente     | Implementazione C generata; TLS richiede un pacchetto corrispondente | Nessuna affermazione di attesa di rete generica completa | Non supportato                    |

Le compilazioni predefinite richiedono la toolchain Rust corrispondente e il linker. C nativo richiede una toolchain C per la destinazione. Superare `ku check` non dimostra che uno dei due processi di compilazione funzioni. Un comando LLVM non implica un backend di rilascio completo.

## Il sottoinsieme Task nativo

La versione include il percorso C nativo limitato `async fn main(): null!`: primitive elencate, stringhe, array piatti, Results a un livello, avvio/spostamento/await di Task, parte del controllo di flusso e operazioni limitate su file/cartelle in background. TCP/TLS locale usa un pool bloccante fisso. Non equivale a readiness di rete generale o M:N completo; consulta la documentazione di concorrenza della versione.

Restano incompleti attese di rete Task generiche, esecuzione M:N completa, combinazioni arbitrarie di heap dinamico, tutte le combinazioni di catture/funzioni async e test prolungati completi. Consulta il sottoinsieme esatto accettato e rifiutato nel [contratto di concorrenza](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).

## Limiti di database e rete

Le API di database usano `module.client(config)?`, seguito dai metodi del ricevitore, con un pool limitato nel client. La validazione con server reali è separata per ogni driver, sistema e commit. Un'ABI simulata o un collegamento riuscito non sostituiscono una query reale.

Redis usa TCP in chiaro. MySQL non dispone di una verifica coerente dei certificati con chiusura in caso di errore fra Oracle/MariaDB. Le connessioni dirette dovrebbero quindi usare loopback, reti private controllate o un tunnel TLS verificato. PostgreSQL remoto richiede `sslmode=verify-full` esplicito e certificati radice attendibili. Il TLS nativo in `std.net` non significa che HTTP, Redis o l'interprete riutilizzino quel percorso.

## Self-hosting e compatibilità

Il lexer e il parser self-hosted di Ku hanno sottoinsiemi delimitati e controlli differenziali. Un parser, checker, IR e backend completamente self-hosted restano incompleti; Rust fornisce ancora la toolchain di bootstrap. La versione 0.0.x non promette compatibilità del sorgente o dell'ABI C/FFI generata.

## Ambito e limiti di v0.0.18

- JSON: `encode`/`decode` condividono `stringify`/`parse`, con la correzione di andata e ritorno di `1e20`.
- Task nativo resta il sottoinsieme limitato elencato; ammissione allo shutdown e pulizia non equivalgono a M:N completo o sospensione di rete generale.
- HTTP offre readiness socket, code, risposte e keep-alive limitati e diagnostica limitata opzionale; non implica accettazione di tutti i carichi.
- `ku supervise` controlla worker di programmi interi, ammissione, byte delle code, uscita e pulizia; non un worker per richiesta o Task e nessuna applicazione automatica a `ku run`.

**Problema HTTP noto:** possono verificarsi errori HTTP 504 intermittenti. La causa non è ancora confermata.

### Limiti di memoria dei processi

| Limiti di memoria dei processi | `ku supervise`                                                                                                                                                                                                                                  |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux                          | Memoria contabilizzata da cgroup v2; serve una radice di dominio scrivibile e delegata con `memory`, `pids`, `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max`, `cgroup.kill`. Controlli o permessi mancanti impediscono l’avvio. |
| Windows                        | Job Objects annidati limitano worker e gruppo in memoria virtuale impegnata, non RSS. L’impegno iniziale è verificato prima di liberare il thread applicativo sospeso; le regole dell’host possono rifiutare l’assegnazione al Job.             |
| macOS                          | Isolamento non supportato nativamente e rifiutato prima dell’esecuzione. Usa un host o una VM Linux configurati correttamente; nessun inoltro automatico a VM.                                                                                  |

Il dominio del worker conta allocazioni Ku e librerie esterne. Contabilità cgroup Linux e memoria impegnata Windows sono misure diverse. `memory.max` può consentire eccessi transitori; nessun limite assoluto di tutta la memoria fisica in ogni istante è garantito.

Il coordinatore non ha un limite OS rigido; riserve e contabilità di ammissione non sono un RSS preciso. L’intero heap di una Task non è misurato esattamente; pool, cache e allocazioni di librerie non sono attribuibili con precisione. Database esterni, host/VM, compilatori e normale `ku run` non sono limitati automaticamente dal budget del worker.

Se il sistema termina un worker, viene segnalato errore e le riserve sono restituite solo dopo pulizia confermata. Alcune allocazioni falliscono prima, ma l’esaurimento di librerie esterne non diventa necessariamente un Result ordinario. **Le operazioni database non vengono mai ripetute automaticamente**; l’uscita del worker non prova rollback e gli effetti possono restare ignoti.

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
