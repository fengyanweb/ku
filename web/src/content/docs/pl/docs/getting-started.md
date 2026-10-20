---
title: 'Pierwsze kroki'
description: 'Napisz Hello Ku, sprawdź kod źródłowy i poznaj różnicę między interpretacją a kompilacją.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources: ['examples/hello.ku', 'README.md', 'docs/syntax.md', 'docs/v0.0.18.md']
---

Dokumentacja opisuje zamrożony zakres eksperymentalnego wydania wstępnego v0.0.18. Faktyczna publikacja i pliki do pobrania są zapisane w GitHub Releases. Ta strona używa podstawowej składni i poleceń CLI dostępnych już w v0.0.17. Najpierw [zainstaluj Ku](/pl/docs/install/) i sprawdź, czy działa `ku --version`.

## Pierwszy program

Utwórz `hello.ku` o tej samej zawartości co `examples/hello.ku` w repozytorium:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` to punkt wejścia. `println` wypisuje jeden wiersz tekstu. Ku nie używa `let` ani `let mut`.

## Sprawdzanie i uruchamianie

W katalogu pliku wykonaj:

```sh
ku check hello.ku
ku run hello.ku
```

`check` sprawdza błędy leksykalne, składniowe i podstawowe semantyczne bez uruchamiania programu. Oczekiwane standardowe wyjście podczas uruchomienia:

```text
Hello Ku
```

Jeżeli Ku nie ma w `PATH`, wywołaj plik wykonywalny pełną ścieżką instalacji. W Windows plik nazywa się `ku.exe`.

## Zmienne i funkcje

Ten fragment podstawowej składni można umieścić wewnątrz `main`:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

Pierwsze przypisanie deklaruje zmienną. Jawny typ zapisuje się jako `name:type = value`. `print` wypisuje bez automatycznego nowego wiersza; `println` go dodaje.

## Budowanie pliku wykonywalnego

Strona instalacji wymienia wymagane narzędzia. Dla domyślnego zaplecza:

```sh
ku build hello.ku
```

Domyślne zaplecze pakuje program uruchamiający interpreter. Natywna kompilacja przez C wymaga osobnego kompilatora C:

```sh
ku build --backend c hello.ku
```

Domyślne wyniki znajdują się w `.ku/build/<profile>/`; użyj dokładnej ścieżki podanej przez CLI. Zaplecza obsługują różne funkcje. Pomyślny `ku check` nie dowodzi, że budowanie natywne się powiedzie.

Uwaga: `ku build --native hello.ku` **bez `-o` generuje wyłącznie kod źródłowy C**, bez konsolidacji. Przy pierwszym skompilowanym programie użyj powyższego polecenia `--backend c`.

## Tworzenie projektu

Jeżeli potrzebujesz katalogu pakietu, użyj wbudowanego szablonu:

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

Punkt wejścia pakietu konfiguruje się w `ku.mod`. `ku create --list` wyświetla dostępne szablony. Polecenia `ku test` i `ku fmt` obecnie nie istnieją.

Przejdź do [przewodnika po języku](/pl/docs/guide/language/), [rozwiązywania problemów](/pl/docs/guide/errors/) i [obsługi zapleczy](/pl/docs/support/). Pokazane wyniki są oczekiwanymi przykładami i nie zastępują sprawdzenia instalacji na własnej platformie.
