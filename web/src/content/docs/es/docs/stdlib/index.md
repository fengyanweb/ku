---
title: 'Biblioteca estándar'
description: 'Encuentra módulos de la biblioteca estándar de Ku, firmas esenciales y límites de los backends.'
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

Esta documentación describe el alcance fijado de la versión preliminar experimental v0.0.18. La publicación real y las descargas constan en GitHub Releases. Estas interfaces esenciales se han contrastado con el código fuente; no es una referencia exhaustiva de API. El [contrato de sintaxis](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) define el comportamiento. Consulta los límites de los backends en la [matriz de compatibilidad](/es/docs/support/).

## Importa módulos

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes` y `net` requieren importaciones explícitas. Algunos módulos antiguos aún pueden invocarse directamente; los ejemplos nuevos prefieren importaciones explícitas para aclarar las dependencias. `std.task` es un espacio de nombres interno para observación y pruebas de estrés, no una API de concurrencia para aplicaciones.

| Módulo                               | Finalidad                                                    | Límites que debes recordar                                                                                       |
| ------------------------------------ | ------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------- |
| `std.string` / métodos de cadenas    | Longitud Unicode, longitud en bytes, búsqueda, segmentos     | `trim/lower/upper` nativos carecen de implementación Unicode                                                     |
| `std.array` / métodos de arrays      | Longitud, búsqueda, nuevos arrays, map                       | El fallo de un índice directo difiere del `try_get` recuperable                                                  |
| `std.object`                         | Búsqueda con alternativa explícita mediante `get_or`         | Un índice normal ausente no devuelve null de forma predeterminada                                                |
| [std.json](/es/docs/stdlib/json/)    | Codificación y decodificación estrictas de JSON              | No admitidas en Tasks nativos ni LLVM                                                                            |
| `std.fs`                             | Archivos, comprobación de existencia, listado de directorios | Las rutas relativas se resuelven desde el directorio del código `.ku` actual                                     |
| `std.time`                           | Hora, fechas, duraciones, pausas                             | Las esperas síncronas y de Tasks utilizan rutas de ejecución diferentes                                          |
| `std.http`                           | Clientes HTTP, servicios, respuestas                         | Persisten diferencias de backend y ciclo de vida; no es una implementación de red genérica y completa para Tasks |
| `std.config`                         | Entorno y configuración                                      | No guardar credenciales reales en el código versionado                                                           |
| `std.pg` / `std.redis` / `std.mysql` | Clientes unificados con pools limitados                      | Solo nativos; la validación con servidores reales y los límites TLS requieren comprobaciones independientes      |
| `std.bytes` / `std.net`              | Datos binarios, cliente TCP, TLS opcional                    | Ruta C nativa generada; TLS requiere un paquete de destino correspondiente                                       |
| `std.lexer` / `std.parser`           | Ayudas para depurar el compilador                            | Devuelven resúmenes de depuración, no estructuras públicas completas de Token / AST                              |

## Firmas habituales

Estas son firmas documentales. `&` indica un modo de parámetro de solo lectura admitido; `T!` indica un resultado recuperable:

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

`"中😀".len()` es 2 y `.byte_len()` es 7. La longitud cuenta valores escalares Unicode; los segmentos utilizan índices de caracteres, no bytes UTF-8. `array.push` y `array.concat` devuelven arrays nuevos sin modificar sus entradas.

## Lee un archivo

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Primero crea un archivo UTF-8 `message.txt` junto al código fuente. Si el archivo no existe o falla la lectura, se devuelve un error. Este ejemplo depende de ese archivo de entrada. `try_read` y `try_write` siguen siendo alias compatibles con los mismos tipos de resultado.

`read_dir` devuelve rutas ordenadas con propiedad independiente, limitadas a 10.000 entradas y 1.000.000 de bytes de ruta en total. Las operaciones de archivos del subconjunto nativo restringido de Tasks utilizan un pool fijo en segundo plano. Esto no acredita esperas de red genéricas mediante el mismo mecanismo.

## Mantenimiento de la referencia

Los metadatos de las interfaces públicas están en [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/stdlib/metadata.rs). Se comprueban junto a implementaciones y contratos. El sitio mantiene actualmente una referencia manual seleccionada; no se ha confirmado la existencia de un comando `ku doc`.

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
