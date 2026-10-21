---
title: 'Biblioteca standard'
description: 'Găsește modulele bibliotecii standard Ku, semnăturile de bază și limitele backendurilor.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
  ]
---

Documentația descrie domeniul fixat al prelansării experimentale v0.0.18. Publicarea efectivă și descărcările sunt înregistrate în GitHub Releases. Aceste puncte de intrare principale au fost verificate în sursă; aceasta nu este o referință API exhaustivă. [Contractul de sintaxă](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) definește comportamentul. Vezi [matricea de suport](/ro/docs/support/) pentru limitele backendurilor.

## Importarea modulelor

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes` și `net` necesită importuri explicite. Unele module istorice pot fi încă apelate direct; exemplele noi preferă importurile explicite pentru a clarifica dependențele. `std.task` este un spațiu de nume intern pentru observare și teste de stres, nu un API de concurență pentru aplicații.

| Modul                                | Scop                                                    | Limite de reținut                                                                                       |
| ------------------------------------ | ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `std.string` / metode de șir         | Lungime Unicode, lungime în octeți, căutare, secționare | `trim/lower/upper` native nu au implementare Unicode                                                    |
| `std.array` / metode de tablou       | Lungime, căutare, tablouri noi, map                     | Eșecul indexării directe diferă de `try_get` recuperabil                                                |
| `std.object`                         | Căutare cu valoare alternativă explicită prin `get_or`  | Un index obișnuit absent nu devine implicit null                                                        |
| [std.json](/ro/docs/stdlib/json/)    | Codificare și decodificare JSON stricte                 | Nesuportate în Task nativ și LLVM                                                                       |
| `std.fs`                             | Fișiere, verificări de existență, listare directoare    | Căile relative sunt rezolvate din directorul sursei `.ku` curente                                       |
| `std.time`                           | Timp, date, durate, pauze                               | Așteptările sincrone și Task folosesc căi de execuție diferite                                          |
| `std.http`                           | Clienți HTTP, servicii, răspunsuri                      | Persistă diferențe de backend și ciclu de viață; nu este o implementare completă de rețea generică Task |
| `std.config`                         | Mediu și configurație                                   | Nu include credențiale reale în surse                                                                   |
| `std.pg` / `std.redis` / `std.mysql` | Clienți unificați cu pooluri limitate                   | Doar nativ; serverul real și limitele TLS necesită verificări separate                                  |
| `std.bytes` / `std.net`              | Date binare, client TCP, TLS opțional                   | Cale C nativ generat; TLS necesită pachetul corespunzător țintei                                        |
| `std.lexer` / `std.parser`           | Ajutoare de depanare a compilatorului                   | Returnează rezumate de depanare, nu structuri publice complete Token / AST                              |

## Semnături uzuale

Acestea sunt semnături de documentație. `&` indică modul suportat de parametru doar pentru citire; `T!` indică un rezultat recuperabil:

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

`"中😀".len()` este 2 și `.byte_len()` este 7. Lungimea numără valori scalare Unicode; secționarea folosește indici de caractere, nu octeți UTF-8. `array.push` și `array.concat` returnează tablouri noi fără a modifica intrările.

## Citirea unui fișier

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Creează mai întâi un `message.txt` UTF-8 lângă sursă. Un fișier absent sau o citire nereușită returnează eroare. Exemplul depinde de acel fișier de intrare. `try_read` și `try_write` rămân aliasuri compatibile cu aceleași tipuri de rezultat.

`read_dir` returnează căi sortate, cu proprietate independentă, limitate la 10.000 de intrări și 1.000.000 de octeți de cale în total. Operațiile restrânse de fișier din Task nativ folosesc un pool fix de fundal. Aceasta nu demonstrează așteptare generică de rețea prin același mecanism.

## Întreținerea referinței

Metadatele interfeței publice sunt în [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/stdlib/metadata.rs). Sunt verificate împreună cu implementările și contractele. Site-ul întreține momentan o referință selectată manual; nu a fost confirmată o comandă `ku doc` existentă.

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
