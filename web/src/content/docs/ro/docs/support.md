---
title: 'Suport pentru platforme și backenduri'
description: 'Deosebește platformele Ku publicate, interpretorul, lansatorul, C nativ, submulțimea Task și suportul LLVM.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.19.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
---

Documentația descrie domeniul fixat al prelansării experimentale v0.0.18. Publicarea efectivă și descărcările sunt înregistrate în GitHub Releases.

## Platforme CLI publicate

Versiunea preliminară experimentală v0.0.18 acoperă Windows x64 (MSVC), Linux x64 (GNU) și macOS arm64 (Apple Silicon). Fiecare platformă are propria arhivă. Nu există un singur executabil pentru toate cele trei sisteme. Nu au fost verificate alte fișiere de platformă; vezi [instalarea](/ro/docs/install/).

## Căi de execuție

| Capabilitate                                          | Interpretor `ku run`             | Lansator implicit `ku build`               | C nativ sincron                                       | Task nativ în dezvoltare                               | Prototip textual LLVM          |
| ----------------------------------------------------- | -------------------------------- | ------------------------------------------ | ----------------------------------------------------- | ------------------------------------------------------ | ------------------------------ |
| Limbaj de bază, funcții, controlul execuției          | Implementat                      | Calea interpretorului                      | Submulțime implementată                               | Submulțime tipizată restrânsă                          | Prototip limitat               |
| JSON                                                  | Suportat                         | Suportat                                   | Suportat                                              | Nesuportat                                             | Nesuportat                     |
| Graf complet de importuri locale independent de surse | Încarcă surse                    | Nesuportat; doar intrarea este încorporată | Extins la generare                                    | Extins în submulțimea suportată                        | Fără promisiune de distribuire |
| async / await                                         | Implementat cu limite de resurse | Calea interpretorului                      | Submulțime dedicată sau respingere                    | Parțial, nu async complet                              | Fără suport complet            |
| Clienți PostgreSQL / Redis / MySQL                    | Fără acces la baze de date       | Fără acces interpretat la baze de date     | Doar nativ                                            | Nu așteptare generală Task                             | Nesuportat                     |
| Client TCP / TLS                                      | Fără cale nativă corespunzătoare | Fără cale nativă corespunzătoare           | Implementare C generată; TLS necesită pachet potrivit | Fără afirmație de așteptare generică completă de rețea | Nesuportat                     |

Compilările implicite necesită instrumentele Rust și linkerul potrivite. C nativ necesită instrumente C pentru țintă. Trecerea `ku check` nu dovedește că funcționează vreo compilare. O comandă LLVM nu implică un backend complet de versiune publicată.

## Submulțimea nativă Task

Ramura curentă de dezvoltare are o cale de sursă restrânsă `async fn main(): null!`. Acoperă primitive enumerate explicit, șiruri, tablouri plate, Results cu un singur nivel, pornire/mutare/await Task, unele structuri de control și operații limitate de fișiere/directoare în fundal. Nu este un backend async complet în pachetul publicat v0.0.18.

Așteptarea generică de rețea Task, execuția M:N completă, combinațiile arbitrare de heap dinamic, toate combinațiile de capturi/funcții async și testarea completă de durată rămân neterminate. Vezi submulțimea exactă acceptată și respinsă în [contractul de concurență](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/concurrency.md).

## Limite ale bazelor de date și rețelei

API-urile de baze de date folosesc `module.client(config)?`, urmat de metode ale receptorului, cu un pool limitat în client. Verificarea serverului real este separată pentru fiecare driver, sistem și commit. Un ABI simulat sau o linkare reușită nu înlocuiește o interogare reală.

Redis folosește TCP necriptat. MySQL nu oferă verificare consecventă a certificatelor cu refuz obligatoriu la eroare în Oracle/MariaDB. Conexiunile directe ar trebui, prin urmare, să folosească loopback, rețele private controlate sau un tunel TLS verificat. PostgreSQL la distanță necesită explicit `sslmode=verify-full` și certificate rădăcină de încredere. TLS nativ în `std.net` nu înseamnă că HTTP, Redis sau interpretorul reutilizează acea cale.

## Autoimplementare și compatibilitate

Analizorul lexical și parserul scrise în Ku au submulțimi limitate și verificări diferențiale. Un parser, verificator, IR și backend complet autoimplementate rămân neterminate; Rust furnizează încă instrumentele de bootstrap. Versiunea 0.0.x nu promite compatibilitate a sursei sau a ABI C/FFI generat.

Jurnalele de lucru și rezultatele CI anterioare confirmă doar scopul și commitul declarate. Folosește [notele versiunilor](/ro/releases/) pentru a separa schimbările publicate de dezvoltare. Acest site nu declară trecute toate verificările CI ale ramurii curente, sarcinile de producție sau acceptarea bazelor de date.

> Actualizările v0.0.18 de mai jos sunt momentan în engleză; restul traducerii este păstrat. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

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

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.
