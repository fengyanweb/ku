---
title: 'Obsługa platform i zapleczy'
description: 'Rozróżnij platformy wydań Ku, interpreter, program uruchamiający, natywne C, podzbiór Task i LLVM.'
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
slug: pl/v0.0.18/docs/support
---

## Opublikowane platformy CLI

Eksperymentalne wydanie wstępne v0.0.18 obejmuje Windows x64 (MSVC), Linux x64 (GNU) i macOS arm64 (Apple Silicon). Każda platforma ma własne archiwum. Nie ma jednego pliku wykonywalnego dla wszystkich trzech systemów. Nie sprawdzono innych zasobów platformowych; zobacz [instalację](/pl/v0.0.18/docs/install/).

## Ścieżki wykonania

| Możliwość                                            | Interpreter `ku run`                 | Domyślny program uruchamiający `ku build`    | Synchroniczne natywne C                                 | Natywny Task (podzbiór)                                | Tekstowy prototyp LLVM |
| ---------------------------------------------------- | ------------------------------------ | -------------------------------------------- | ------------------------------------------------------- | ------------------------------------------------------ | ---------------------- |
| Podstawy języka, funkcje, przepływ sterowania        | Zaimplementowane                     | Ścieżka interpretera                         | Zaimplementowany podzbiór                               | Ograniczony typowany podzbiór                          | Ograniczony prototyp   |
| JSON                                                 | Obsługiwane                          | Obsługiwane                                  | Obsługiwane                                             | Nieobsługiwane                                         | Nieobsługiwane         |
| Kompletny lokalny graf importów niezależny od źródeł | Ładuje źródła                        | Nieobsługiwany; osadzany tylko punkt wejścia | Rozwijany podczas generowania                           | Rozwijany w obsługiwanym podzbiorze                    | Bez obietnic wdrożenia |
| async / await                                        | Zaimplementowane z limitami zasobów  | Ścieżka interpretera                         | Osobny podzbiór lub odrzucenie                          | Częściowo, niepełne async                              | Bez pełnej obsługi     |
| Klienty PostgreSQL / Redis / MySQL                   | Bez dostępu do bazy                  | Bez interpretowanego dostępu do bazy         | Tylko natywne                                           | Bez ogólnego oczekiwania Task                          | Nieobsługiwane         |
| Klient TCP / TLS                                     | Brak odpowiadającej ścieżki natywnej | Brak odpowiadającej ścieżki natywnej         | Generowana implementacja C; TLS wymaga zgodnego pakietu | Bez deklaracji pełnego ogólnego oczekiwania sieciowego | Nieobsługiwane         |

Domyślne budowanie wymaga odpowiednich narzędzi Rust i konsolidatora. Natywne C wymaga docelowego zestawu narzędzi C. Pomyślny `ku check` nie dowodzi działania żadnej z kompilacji. Polecenie LLVM nie oznacza kompletnego zaplecza wydaniowego.

## Natywny podzbiór Task

Wydanie obejmuje ograniczoną natywną ścieżkę C `async fn main(): null!`: wymienione typy proste, łańcuchy, płaskie tablice, jednopoziomowe Results, start/przenoszenie/await Task, wybrany przepływ sterowania i ograniczone operacje plików/katalogów w tle. Lokalne TCP/TLS używa stałej puli blokującej. Nie jest to ogólna gotowość sieci ani pełne M:N; dokładny zakres opisuje dokumentacja współbieżności tej wersji.

Ogólne sieciowe oczekiwanie Task, pełne wykonanie M:N, dowolne połączenia dynamicznej sterty, wszystkie kombinacje przechwyceń/funkcji async i pełne długotrwałe testy pozostają nieukończone. Dokładny akceptowany i odrzucany podzbiór opisuje [kontrakt współbieżności](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).

## Ograniczenia baz danych i sieci

API baz danych używają `module.client(config)?`, a następnie metod odbiorcy, z ograniczoną pulą wewnątrz klienta. Weryfikacja rzeczywistego serwera jest osobna dla każdego sterownika, systemu i commitu. Pozorowany ABI lub udana konsolidacja nie zastępują rzeczywistego zapytania.

Redis używa niezaszyfrowanego TCP. MySQL nie zapewnia spójnej weryfikacji certyfikatów z bezwzględnym odrzuceniem błędu w Oracle/MariaDB. Bezpośrednie połączenia powinny więc używać loopback, kontrolowanych sieci prywatnych lub zweryfikowanego tunelu TLS. Zdalny PostgreSQL wymaga jawnego `sslmode=verify-full` i zaufanych certyfikatów głównych. Natywny TLS w `std.net` nie oznacza, że HTTP, Redis lub interpreter używają tej samej ścieżki.

## Samodzielna implementacja i zgodność

Lekser i parser napisane w Ku mają ograniczone podzbiory i testy różnicowe. Kompletny własny parser, moduł sprawdzający, IR i zaplecze nadal nie są ukończone; narzędzia rozruchowe nadal zapewnia Rust. Wersja 0.0.x nie obiecuje zgodności źródeł ani generowanego C/FFI ABI.

## Zakres i limity v0.0.18

- JSON: `encode`/`decode` współdzielą `stringify`/`parse`, z poprawką powrotnego odczytu `1e20`.
- Natywny Task pozostaje wymienionym ograniczonym podzbiorem; poprawki przyjmowania przy zamykaniu i porządkowania nie oznaczają pełnego M:N ani ogólnego oczekiwania sieciowego.
- HTTP zapewnia ograniczoną gotowość socketów, kolejki, odpowiedzi, keep-alive i opcjonalną ograniczoną diagnostykę; nie oznacza to pełnej akceptacji obciążeń.
- `ku supervise` kontroluje workers całych programów, przyjęcia, bajty kolejek, wyjście i sprzątanie; nie worker na żądanie lub Task ani automatyczne ograniczenie `ku run`.

**Znany problem HTTP:** sporadycznie mogą wystąpić błędy HTTP 504. Przyczyna nie została jeszcze potwierdzona.

### Limity pamięci procesów

| Limity pamięci procesów | `ku supervise`                                                                                                                                                                                                                            |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux                   | Pamięć naliczana przez cgroup v2; potrzebny zapisywalny delegowany korzeń domeny z `memory`, `pids`, `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max`, `cgroup.kill`. Brak praw lub kontroli uniemożliwia start aplikacji. |
| Windows                 | Zagnieżdżone Job Objects ograniczają workers i grupę według zadeklarowanej pamięci wirtualnej commit, nie RSS. Początkowy commit jest sprawdzany przed zwolnieniem wstrzymanego wątku; polityka hosta może odmówić przypisania Job.       |
| macOS                   | Brak natywnego wsparcia izolacji; odmowa przed uruchomieniem. W razie potrzeby użyj skonfigurowanego hosta lub VM Linux; brak automatycznego przekierowania do VM.                                                                        |

Domena worker nalicza alokacje Ku i zewnętrznych bibliotek. Naliczanie Linux cgroup i Windows commit to różne miary. `memory.max` może pozwolić na chwilowe przekroczenie; brak gwarancji absolutnego pułapu całej pamięci fizycznej w każdej chwili.

Koordynator nie ma twardego limitu OS; rezerwy i księgowanie przyjęć nie są dokładnym RSS. Pełny heap pojedynczej Task nie jest precyzyjnie mierzony, a pule, cache i biblioteki nie są dokładnie przypisywane. Bazy zewnętrzne, host/VM, kompilatory i zwykły `ku run` nie podlegają automatycznie limitowi worker.

Gdy OS zakończy worker, koordynator zgłasza błąd i oddaje rezerwy dopiero po potwierdzonej obsłudze porządkowej. Niektóre alokacje zawodzą wcześniej, ale wyczerpanie bibliotek nie musi zwracać zwykłego Result. **Operacje baz danych nigdy nie są automatycznie ponawiane**; koniec worker nie dowodzi rollbacku, skutki mogą być nieznane.

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
