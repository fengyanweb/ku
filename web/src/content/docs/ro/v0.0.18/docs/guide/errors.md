---
title: 'Erori și depanare'
description: 'Diagnostichează erorile de instalare Ku, importuri, proprietate, JSON și capabilități ale backendurilor.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
    'docs/v0.0.18-r1.md',
  ]
slug: ro/v0.0.18/docs/guide/errors
---

Diagnosticele compilatorului se pot schimba în dezvoltarea experimentală. Include versiunea CLI, commitul Git, backendul, comanda completă și sursa minimă când raportezi o problemă.

## Identifică executabilul Ku

```sh
ku --version
ku -h
ku check hello.ku
```

Dacă `ku` nu este găsit, apelează executabilul din directorul extras. Folosește `ku.exe` pe Windows. Dacă compilării implicite îi lipsesc Rust sau bibliotecile de linkare, verifică [cerințele instalării](/ro/v0.0.18/docs/install/) și păstrează întregul aranjament al arhivei în loc să copiezi doar CLI.

## Citirea diagnosticelor

Verificarea normală raportează fișierul, poziția în sursă, fragmentul și recomandările disponibile de remediere. Integrările cu instrumente pot folosi:

```sh
ku check --json hello.ku
```

Ieșirea este **JSON Lines**, cu un diagnostic pe linie. Câmpurile includ `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes` și `helps`. Succesul nu produce ieșire; absența ieșirii JSON nu înseamnă că verificarea nu a rulat.

| Diagnostic | Cauză frecventă                           | Ce trebuie făcut                                                                   |
| ---------- | ----------------------------------------- | ---------------------------------------------------------------------------------- |
| `E0105`    | Folosirea `let`                           | Declară variabila prin prima atribuire                                             |
| `E0302`    | Condiție `if` / `while` care nu este bool | Scrie o comparație booleană explicită                                              |
| `E0601`    | Modul necunoscut al bibliotecii standard  | Verifică adevărata cale, precum `std.json`                                         |
| `E0605`    | Import explicit obligatoriu absent        | Importă modulul, de exemplu `import fs from "std.fs"`                              |
| `E0901`    | Citirea unei valori mutate                | Clonează explicit o valoare independentă sau folosește împrumutul sincron suportat |
| `E0913`    | Parametru async împrumutat                | Folosește un parametru deținut; clonează la apelant dacă originalul este necesar   |
| `E0918`    | Scrierea `&value` la apel                 | Transmite un argument obișnuit; semnătura declară împrumutul                       |

Acesta este un rezumat al diagnosticelor înregistrate, nu lista tuturor erorilor. Registrul normativ curent este [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/src/error.rs).

## Erorile de execuție au un contract separat

Valorile `domain/code` ale Error la execuție sunt separate de codurile `E` ale compilatorului. JSON nevalid poate returna `json/parse_error`; o cheie de obiect absentă poate returna `object/missing_key`. Folosește `?` după un apel care poate eșua sau citește câmpurile în `try/catch`.

```ku
import json from "std.json"

fn main() {
    try {
        json.parse("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

Acest program didactic provoacă intenționat o eroare recuperabilă de parsare. Un `byte N` raportat este un decalaj în octeți UTF-8 pornind de la zero. Nu este numărul de caractere din jurul textului chinezesc sau emoji.

## Compilarea nativă este respinsă

Trecerea `ku check` confirmă doar că verificarea frontendului a reușit. C nativ și LLVM au propriile limite. Consultă [matricea de suport](/ro/v0.0.18/docs/support/) când o construcție este respinsă explicit. Mărirea timpului de așteptare sau succesul altui backend nu verifică acea cale nesuportată.

`ku build --native file.ku` fără `-o` scrie doar C. Pentru compilarea și linkarea unui executabil, folosește `ku build --backend c file.ku` și instalează instrumentele C pentru țintă.

Erorile de bază de date `execution_unknown` și `execution_completed_without_result` nu trebuie să declanșeze repetare automată. Păstrează dovezile cererii și operației de afaceri, apoi stabilește starea reală a execuției.

Pentru detalii, vezi [ghidul de diagnostic din depozit](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/diagnostics.md).

Erorile JSON au aceleași domain/code între backenduri, dar message poate diferi. ku run și runner pot adăuga json parse failed:, pe când C nativ nu. Compară domain/code, nu mesajul integral.
