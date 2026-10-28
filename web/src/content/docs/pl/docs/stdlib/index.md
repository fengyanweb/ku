---
title: 'Biblioteka standardowa'
description: 'Moduły biblioteki standardowej Ku, podstawowe sygnatury i ograniczenia zapleczy.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
  ]
---

Dokumentacja opisuje zamrożony zakres eksperymentalnego wydania wstępnego v0.0.18. Faktyczna publikacja i pliki do pobrania są zapisane w GitHub Releases. Te podstawowe punkty wejścia sprawdzono ze źródłami; nie jest to wyczerpujący opis API. Zachowanie definiuje [kontrakt składni](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md). Ograniczenia zapleczy opisuje [macierz obsługi](/pl/docs/support/).

## Importowanie modułów

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes` i `net` wymagają jawnych importów. Niektóre historyczne moduły nadal można wywoływać bezpośrednio; nowe przykłady preferują jawne importy, by uwidocznić zależności. `std.task` jest wewnętrzną przestrzenią nazw obserwacji i testów obciążeniowych, a nie aplikacyjnym API współbieżności.

| Moduł                                | Przeznaczenie                                           | Ograniczenia                                                                                        |
| ------------------------------------ | ------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| `std.string` / metody łańcuchów      | Długość Unicode, długość bajtowa, wyszukiwanie, wycinki | Natywne `trim/lower/upper` nie mają implementacji Unicode                                           |
| `std.array` / metody tablic          | Długość, wyszukiwanie, nowe tablice, map                | Błąd bezpośredniego indeksowania różni się od obsługiwalnego `try_get`                              |
| `std.object`                         | Jawna wartość zastępcza wyszukiwania przez `get_or`     | Brakujący zwykły indeks nie daje domyślnie null                                                     |
| [std.json](/pl/docs/stdlib/json/)    | Ścisłe kodowanie i dekodowanie JSON                     | Nieobsługiwane w natywnym Task i LLVM                                                               |
| `std.fs`                             | Pliki, sprawdzanie istnienia, listowanie katalogu       | Ścieżki względne są rozwiązywane względem katalogu bieżącego źródła `.ku`                           |
| `std.time`                           | Czas, daty, okresy, usypianie                           | Oczekiwanie synchroniczne i Task używa różnych ścieżek wykonania                                    |
| `std.http`                           | Klienty HTTP, usługi, odpowiedzi                        | Pozostają różnice zaplecza i cyklu życia; nie jest to pełna uniwersalna implementacja sieciowa Task |
| `std.config`                         | Środowisko i konfiguracja                               | Nie zapisuj rzeczywistych danych uwierzytelniających w źródłach                                     |
| `std.pg` / `std.redis` / `std.mysql` | Ujednolicone klienty z ograniczonymi pulami             | Tylko natywne; rzeczywisty serwer i ograniczenia TLS wymagają osobnej weryfikacji                   |
| `std.bytes` / `std.net`              | Dane binarne, klient TCP, opcjonalny TLS                | Generowana natywna ścieżka C; TLS wymaga odpowiedniego pakietu docelowego                           |
| `std.lexer` / `std.parser`           | Pomoc przy debugowaniu kompilatora                      | Zwracają podsumowania debugowania, a nie pełne publiczne struktury Token / AST                      |

## Typowe sygnatury

To sygnatury dokumentacyjne. `&` oznacza obsługiwany tryb parametru tylko do odczytu; `T!` — obsługiwalny wynik:

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

`"中😀".len()` wynosi 2, a `.byte_len()` — 7. Długość zlicza wartości skalarne Unicode; wycinanie używa indeksów znaków, a nie bajtów UTF-8. `array.push` i `array.concat` zwracają nowe tablice bez modyfikacji wejściowych.

## Odczyt pliku

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Najpierw utwórz plik UTF-8 `message.txt` obok źródła. Brak pliku lub nieudany odczyt zwraca błąd. Przykład zależy od tego pliku wejściowego. `try_read` i `try_write` pozostają zgodnymi aliasami z tymi samymi typami wyniku.

`read_dir` zwraca posortowane ścieżki o niezależnej własności, ograniczone do 10 000 wpisów i 1 000 000 bajtów ścieżek łącznie. Ograniczone natywne operacje plikowe Task używają stałej puli tła. Nie dowodzi to ogólnego oczekiwania sieciowego tym samym mechanizmem.

## Utrzymywanie dokumentacji

Metadane publicznego interfejsu znajdują się w [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/stdlib/metadata.rs). Są sprawdzane razem z implementacjami i kontraktami. Witryna utrzymuje obecnie ręcznie redagowany opis; nie potwierdzono istniejącego polecenia `ku doc`.

## API reference (English)

Detailed module references and runnable examples are currently available in English:

- [http](/en/docs/stdlib/http/)
- [time](/en/docs/stdlib/time/)
- [fs](/en/docs/stdlib/fs/)
- [config](/en/docs/stdlib/config/)
- [string](/en/docs/stdlib/string/)
- [array](/en/docs/stdlib/array/)
- [object](/en/docs/stdlib/object/)
- [json](/en/docs/stdlib/json/)
- [bytes](/en/docs/stdlib/bytes/)
- [net](/en/docs/stdlib/net/)
- [database](/en/docs/stdlib/database/)
- [compiler](/en/docs/stdlib/compiler/)
