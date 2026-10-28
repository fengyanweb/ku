---
title: 'Primii pași'
description: 'Scrie Hello Ku, verifică sursa și deosebește interpretarea de compilare.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources: ['examples/hello.ku', 'README.md', 'docs/syntax.md', 'docs/v0.0.19.md']
---

Documentația descrie domeniul fixat al prelansării experimentale v0.0.18. Publicarea efectivă și descărcările sunt înregistrate în GitHub Releases. Această pagină folosește sintaxa de bază și comenzi CLI deja existente în v0.0.17. Mai întâi [instalează Ku](/ro/docs/install/) și confirmă că `ku --version` funcționează.

## Primul program

Creează `hello.ku` cu același conținut ca `examples/hello.ku` din depozit:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` este punctul de intrare. `println` scrie o linie de text. Ku nu folosește `let` sau `let mut`.

## Verificare și rulare

Din directorul fișierului:

```sh
ku check hello.ku
ku run hello.ku
```

`check` verifică erorile lexicale, de sintaxă și semantice de bază fără să ruleze programul. Ieșirea standard așteptată la rulare:

```text
Hello Ku
```

Dacă Ku nu este în `PATH`, apelează executabilul folosind calea completă de instalare. Executabilul Windows este `ku.exe`.

## Variabile și funcții

Acest fragment de sintaxă de bază poate fi pus în `main`:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

Prima atribuire declară o variabilă. Un tip explicit folosește `name:type = value`. `print` scrie fără linie nouă automată; `println` o adaugă.

## Construirea unui executabil

Pagina de instalare enumeră instrumentele necesare. Pentru backendul implicit:

```sh
ku build hello.ku
```

Backendul implicit împachetează un lansator al interpretorului. Compilarea nativă prin C necesită un compilator C separat:

```sh
ku build --backend c hello.ku
```

Implicit, rezultatul se află în `.ku/build/<profile>/`; folosește calea exactă raportată de CLI. Cele două backenduri acoperă funcții diferite. Trecerea `ku check` nu dovedește că o compilare nativă va reuși.

Notă: `ku build --native hello.ku` **fără `-o` emite doar sursă C**, fără linkare. Pentru primul program compilat folosește comanda `--backend c` de mai sus.

## Crearea unui proiect

Folosește un șablon integrat când ai nevoie de un director de pachet:

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

Punctul de intrare al pachetului este configurat în `ku.mod`. `ku create --list` enumeră șabloanele disponibile. În prezent nu există comenzile `ku test` sau `ku fmt`.

Continuă cu [ghidul limbajului](/ro/docs/guide/language/), [depanarea](/ro/docs/guide/errors/) și [suportul backendurilor](/ro/docs/support/). Rezultatele de aici sunt exemple așteptate și nu înlocuiesc verificarea instalării pe platforma ta.
