---
title: 'Obsługa platform i zapleczy'
description: 'Rozróżnij platformy wydań Ku, interpreter, program uruchamiający, natywne C, podzbiór Task i LLVM.'
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

Dokumentacja opisuje zamrożony zakres eksperymentalnego wydania wstępnego v0.0.18. Faktyczna publikacja i pliki do pobrania są zapisane w GitHub Releases.

## Opublikowane platformy CLI

Eksperymentalne wydanie wstępne v0.0.18 obejmuje Windows x64 (MSVC), Linux x64 (GNU) i macOS arm64 (Apple Silicon). Każda platforma ma własne archiwum. Nie ma jednego pliku wykonywalnego dla wszystkich trzech systemów. Nie sprawdzono innych zasobów platformowych; zobacz [instalację](/pl/docs/install/).

## Ścieżki wykonania

| Możliwość                                            | Interpreter `ku run`                 | Domyślny program uruchamiający `ku build`    | Synchroniczne natywne C                                 | Rozwojowe natywne Task                                 | Tekstowy prototyp LLVM |
| ---------------------------------------------------- | ------------------------------------ | -------------------------------------------- | ------------------------------------------------------- | ------------------------------------------------------ | ---------------------- |
| Podstawy języka, funkcje, przepływ sterowania        | Zaimplementowane                     | Ścieżka interpretera                         | Zaimplementowany podzbiór                               | Ograniczony typowany podzbiór                          | Ograniczony prototyp   |
| JSON                                                 | Obsługiwane                          | Obsługiwane                                  | Obsługiwane                                             | Nieobsługiwane                                         | Nieobsługiwane         |
| Kompletny lokalny graf importów niezależny od źródeł | Ładuje źródła                        | Nieobsługiwany; osadzany tylko punkt wejścia | Rozwijany podczas generowania                           | Rozwijany w obsługiwanym podzbiorze                    | Bez obietnic wdrożenia |
| async / await                                        | Zaimplementowane z limitami zasobów  | Ścieżka interpretera                         | Osobny podzbiór lub odrzucenie                          | Częściowo, niepełne async                              | Bez pełnej obsługi     |
| Klienty PostgreSQL / Redis / MySQL                   | Bez dostępu do bazy                  | Bez interpretowanego dostępu do bazy         | Tylko natywne                                           | Bez ogólnego oczekiwania Task                          | Nieobsługiwane         |
| Klient TCP / TLS                                     | Brak odpowiadającej ścieżki natywnej | Brak odpowiadającej ścieżki natywnej         | Generowana implementacja C; TLS wymaga zgodnego pakietu | Bez deklaracji pełnego ogólnego oczekiwania sieciowego | Nieobsługiwane         |

Domyślne budowanie wymaga odpowiednich narzędzi Rust i konsolidatora. Natywne C wymaga docelowego zestawu narzędzi C. Pomyślny `ku check` nie dowodzi działania żadnej z kompilacji. Polecenie LLVM nie oznacza kompletnego zaplecza wydaniowego.

## Natywny podzbiór Task

Bieżąca gałąź rozwojowa ma ograniczoną ścieżkę źródłową `async fn main(): null!`. Obejmuje ona jawnie wymienione typy proste, łańcuchy, płaskie tablice, jednopoziomowe Results, uruchamianie/przenoszenie/oczekiwanie Task, niektóre konstrukcje sterowania oraz ograniczone operacje plikowe/katalogowe w tle. Nie jest to kompletny asynchroniczny backend w opublikowanym pakiecie v0.0.18.

Ogólne sieciowe oczekiwanie Task, pełne wykonanie M:N, dowolne połączenia dynamicznej sterty, wszystkie kombinacje przechwyceń/funkcji async i pełne długotrwałe testy pozostają nieukończone. Dokładny akceptowany i odrzucany podzbiór opisuje [kontrakt współbieżności](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md).

## Ograniczenia baz danych i sieci

API baz danych używają `module.client(config)?`, a następnie metod odbiorcy, z ograniczoną pulą wewnątrz klienta. Weryfikacja rzeczywistego serwera jest osobna dla każdego sterownika, systemu i commitu. Pozorowany ABI lub udana konsolidacja nie zastępują rzeczywistego zapytania.

Redis używa niezaszyfrowanego TCP. MySQL nie zapewnia spójnej weryfikacji certyfikatów z bezwzględnym odrzuceniem błędu w Oracle/MariaDB. Bezpośrednie połączenia powinny więc używać loopback, kontrolowanych sieci prywatnych lub zweryfikowanego tunelu TLS. Zdalny PostgreSQL wymaga jawnego `sslmode=verify-full` i zaufanych certyfikatów głównych. Natywny TLS w `std.net` nie oznacza, że HTTP, Redis lub interpreter używają tej samej ścieżki.

## Samodzielna implementacja i zgodność

Lekser i parser napisane w Ku mają ograniczone podzbiory i testy różnicowe. Kompletny własny parser, moduł sprawdzający, IR i zaplecze nadal nie są ukończone; narzędzia rozruchowe nadal zapewnia Rust. Wersja 0.0.x nie obiecuje zgodności źródeł ani generowanego C/FFI ABI.

Dzienniki prac i wcześniejsze wyniki CI potwierdzają wyłącznie podany w nich zakres i commit. [Informacje o wydaniach](/pl/releases/) odróżniają opublikowane zmiany od prac rozwojowych. Ta witryna nie ogłasza, że całe CI bieżącej gałęzi, obciążenia produkcyjne lub akceptacja baz danych przeszły pomyślnie.

> Nowe informacje o v0.0.18 są na razie podane poniżej po angielsku; pozostałe tłumaczenie zachowano. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

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
