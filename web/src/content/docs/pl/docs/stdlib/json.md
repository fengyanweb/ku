---
title: 'Dokumentacja JSON'
description: 'Kodowanie i dekodowanie JSON w Ku, konwersja KuValue, kody błędów i obsługa zapleczy.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
  ]
---

> Nowe informacje o v0.0.18 są na razie podane poniżej po angielsku; pozostałe tłumaczenie zachowano. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## Import i interfejsy

```ku
import json from "std.json"
```

| Interfejs                    | Zwraca     | Znaczenie                                                         |
| ---------------------------- | ---------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Kodowanie tylko do odczytu do nowego zwartego tekstu JSON         |
| `json.parse(text:str)`       | `KuValue!` | Ścisłe parsowanie do dynamicznej wartości o niezależnej własności |
| `json.try_parse(text:str)`   | `KuValue!` | Zgodny alias `parse`                                              |
| `json.encode(&value:any)`    | `str!`     | Alias `stringify` z gałęzi rozwojowej                             |
| `json.decode(text:str)`      | `KuValue!` | Alias `parse` z gałęzi rozwojowej                                 |

`!` oznacza Result. Użyj `?` do propagowania błędów lub `try/catch` do ich obsługi. Kodowanie nie zużywa ani nie zmienia wejścia.

## Kodowanie i odczyt wartości

To kompletny podstawowy przykład. Gałąź rozwojowa zawiera szerszy przykład regresyjny w [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

Klucze obiektu są sortowane deterministycznie. Oczekiwany wynik:

```text
{"count":7,"name":"Ku"}
Ku
8
```

Sparsowany `KuValue` nie może bezpośrednio uczestniczyć w arytmetyce całkowitej. Najpierw użyj `.as_int()?` lub `.as_str()?`. Niezgodny typ zwraca błąd. Brakujące klucze obiektu również zwracają błędy zamiast automatycznych wartości domyślnych.

## Dane i granice

Obsługiwane wejście obejmuje łańcuchy, wartości logiczne, liczby całkowite, skończone liczby zmiennoprzecinkowe, null, zagnieżdżone tablice, obiekty z kluczami tekstowymi i reprezentowalne struktury. Nie można kodować funkcji, Tasks, uchwytów zasobów, Results, wyliczeń ani cykli.

| Reguła                    | Bieżąca implementacja                                                                                  |
| ------------------------- | ------------------------------------------------------------------------------------------------------ |
| Składnia JSON             | Ścisła; odrzuca komentarze, końcowe przecinki, dodatkowy tekst po wartości i niepoprawny Unicode       |
| Wejście / wyjście         | Każde ograniczone do 1 000 000 bajtów                                                                  |
| Budżet głębokości         | Głębokość korzenia wynosi 0; odrzuca `depth > 32`; przyjmuje 33 poziomy pustych kontenerów, odrzuca 34 |
| Liczby całkowite          | 64-bitowe ze znakiem; przepełnienie jest błędem, bez automatycznej konwersji na float                  |
| Zmienny przecinek         | Skończone binary64; oryginalny zapis, znaczniki float i ujemne zero nie są zachowywane                 |
| Unicode                   | Tekst chiński, emoji, sekwencje ucieczki, osadzony NUL                                                 |
| Powtórzone klucze obiektu | Parsowanie zachowuje ostatnią wartość                                                                  |
| Kolejność kluczy          | Kodowanie sortuje według kolejności bajtów UTF-8                                                       |

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## Błędy

Obsługiwalne błędy używają domeny `json`. Typowe kody to `parse_error`, `stringify_error` i `out_of_memory`. `byte N` w komunikacie parsowania jest przesunięciem bajtowym UTF-8 od zera. Minimalną obsługę niepoprawnego wejścia pokazują [błędy i rozwiązywanie problemów](/pl/docs/guide/errors/).

## Obsługa zapleczy

Interpreter, domyślny pakowany program uruchamiający i synchroniczne natywne zaplecze C mają implementacje JSON. **Ograniczony natywny podzbiór Task i LLVM nie obsługują JSON.** Importy domyślnego programu uruchamiającego nadal zależą od położenia źródeł; obsługa JSON nie oznacza w pełni niezależnego pakietu.

Sygnatury i ograniczenia tej strony sprawdzono z kodem i kontraktami. Weryfikacja wykonania musi podawać wersję, commit i zaplecze. Zobacz pełny [kontrakt składni JSON](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md#127-json).
