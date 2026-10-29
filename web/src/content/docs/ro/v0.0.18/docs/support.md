---
title: 'Suport pentru platforme și backenduri'
description: 'Deosebește platformele Ku publicate, interpretorul, lansatorul, C nativ, submulțimea Task și suportul LLVM.'
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
slug: ro/v0.0.18/docs/support
---

## Platforme CLI publicate

Versiunea preliminară experimentală v0.0.18 acoperă Windows x64 (MSVC), Linux x64 (GNU) și macOS arm64 (Apple Silicon). Fiecare platformă are propria arhivă. Nu există un singur executabil pentru toate cele trei sisteme. Nu au fost verificate alte fișiere de platformă; vezi [instalarea](/ro/v0.0.18/docs/install/).

## Căi de execuție

| Capabilitate                                          | Interpretor `ku run`             | Lansator implicit `ku build`               | C nativ sincron                                       | Task nativ (submulțime)                                | Prototip textual LLVM          |
| ----------------------------------------------------- | -------------------------------- | ------------------------------------------ | ----------------------------------------------------- | ------------------------------------------------------ | ------------------------------ |
| Limbaj de bază, funcții, controlul execuției          | Implementat                      | Calea interpretorului                      | Submulțime implementată                               | Submulțime tipizată restrânsă                          | Prototip limitat               |
| JSON                                                  | Suportat                         | Suportat                                   | Suportat                                              | Nesuportat                                             | Nesuportat                     |
| Graf complet de importuri locale independent de surse | Încarcă surse                    | Nesuportat; doar intrarea este încorporată | Extins la generare                                    | Extins în submulțimea suportată                        | Fără promisiune de distribuire |
| async / await                                         | Implementat cu limite de resurse | Calea interpretorului                      | Submulțime dedicată sau respingere                    | Parțial, nu async complet                              | Fără suport complet            |
| Clienți PostgreSQL / Redis / MySQL                    | Fără acces la baze de date       | Fără acces interpretat la baze de date     | Doar nativ                                            | Nu așteptare generală Task                             | Nesuportat                     |
| Client TCP / TLS                                      | Fără cale nativă corespunzătoare | Fără cale nativă corespunzătoare           | Implementare C generată; TLS necesită pachet potrivit | Fără afirmație de așteptare generică completă de rețea | Nesuportat                     |

Compilările implicite necesită instrumentele Rust și linkerul potrivite. C nativ necesită instrumente C pentru țintă. Trecerea `ku check` nu dovedește că funcționează vreo compilare. O comandă LLVM nu implică un backend complet de versiune publicată.

## Submulțimea nativă Task

Versiunea include calea C nativă restrânsă `async fn main(): null!`: primitive enumerate, șiruri, tablouri plate, Results cu un nivel, pornire/mutare/await Task, o parte a controlului și operații limitate de fișiere/directoare în fundal. TCP/TLS local folosește un pool blocant fix. Nu este readiness generală de rețea sau M:N complet; consultă referința de concurență a versiunii.

Așteptarea generică de rețea Task, execuția M:N completă, combinațiile arbitrare de heap dinamic, toate combinațiile de capturi/funcții async și testarea completă de durată rămân neterminate. Vezi submulțimea exactă acceptată și respinsă în [contractul de concurență](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).

## Limite ale bazelor de date și rețelei

API-urile de baze de date folosesc `module.client(config)?`, urmat de metode ale receptorului, cu un pool limitat în client. Verificarea serverului real este separată pentru fiecare driver, sistem și commit. Un ABI simulat sau o linkare reușită nu înlocuiește o interogare reală.

Redis folosește TCP necriptat. MySQL nu oferă verificare consecventă a certificatelor cu refuz obligatoriu la eroare în Oracle/MariaDB. Conexiunile directe ar trebui, prin urmare, să folosească loopback, rețele private controlate sau un tunel TLS verificat. PostgreSQL la distanță necesită explicit `sslmode=verify-full` și certificate rădăcină de încredere. TLS nativ în `std.net` nu înseamnă că HTTP, Redis sau interpretorul reutilizează acea cale.

## Autoimplementare și compatibilitate

Analizorul lexical și parserul scrise în Ku au submulțimi limitate și verificări diferențiale. Un parser, verificator, IR și backend complet autoimplementate rămân neterminate; Rust furnizează încă instrumentele de bootstrap. Versiunea 0.0.x nu promite compatibilitate a sursei sau a ABI C/FFI generat.

## Domeniul și limitele v0.0.18

- JSON: `encode`/`decode` împart `stringify`/`parse`, cu corecția dus-întors pentru `1e20`.
- Task nativ rămâne submulțimea limitată enumerată; admiterea la oprire și curățarea nu înseamnă M:N complet sau suspendare generală de rețea.
- HTTP oferă readiness de socket, cozi, răspunsuri și keep-alive limitate și diagnostice limitate opționale; nu înseamnă acceptarea tuturor sarcinilor.
- `ku supervise` controlează workers de programe întregi, admitere, octeții cozilor, ieșire și curățare; nu worker per cerere sau Task și nici aplicare automată la `ku run`.

**Problemă HTTP cunoscută:** pot apărea erori HTTP 504 intermitente. Cauza nu a fost încă confirmată.

### Limitele memoriei proceselor

| Limitele memoriei proceselor | `ku supervise`                                                                                                                                                                                                                                                      |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux                        | Memorie contabilizată de cgroup v2; necesită rădăcină de domeniu delegată și inscriptibilă cu `memory`, `pids`, `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max`, `cgroup.kill`. Lipsa permisiunilor sau controalelor împiedică pornirea aplicației. |
| Windows                      | Job Objects imbricate limitează workers și grupul prin memorie virtuală angajată, nu RSS. Angajamentul inițial este verificat înainte de eliberarea firului suspendat; politica gazdei poate respinge atribuirea Job.                                               |
| macOS                        | Izolarea nu este suportată nativ și este respinsă înainte de pornire. Folosește gazdă sau VM Linux configurată corect; fără transfer automat în VM.                                                                                                                 |

Domeniul worker contabilizează alocările Ku și bibliotecilor externe. cgroup Linux și memoria angajată Windows sunt măsuri diferite. `memory.max` poate permite depășiri tranzitorii; nu există plafon absolut al întregii memorii fizice în fiecare moment.

Coordonatorul nu are limită OS strictă; rezervele și admiterea nu sunt RSS precis. Heapul complet al unei Task nu este măsurat exact, iar poolurile, cache-urile și bibliotecile nu pot fi atribuite precis. Bazele externe, gazda/VM, compilatoarele și `ku run` obișnuit nu sunt automat limitate de bugetul worker.

Când OS termină un worker, coordonatorul raportează eșec și restituie rezervele numai după curățare confirmată. Unele alocări pot eșua anticipat, dar epuizarea bibliotecilor nu devine obligatoriu Result obișnuit. **Operațiile de bază de date nu se repetă automat**; ieșirea worker nu dovedește rollback, iar efectele pot rămâne necunoscute.

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
