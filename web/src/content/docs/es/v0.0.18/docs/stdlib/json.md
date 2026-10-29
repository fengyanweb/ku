---
title: 'Referencia de JSON'
description: 'Codificación y decodificación JSON en Ku, conversión de KuValue, códigos de error y compatibilidad de backends.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
slug: es/v0.0.18/docs/stdlib/json
---

`std.json` codifica y decodifica JSON. `json.encode` toma la entrada prestada en solo lectura; `json.decode` crea un `KuValue` de propiedad independiente. Ambos devuelven Result. `stringify`, `parse` y `try_parse` siguen siendo compatibles.

## Importación e interfaces

```ku
import json from "std.json"
```

| Interfaz                     | Devuelve   | Significado                                                        |
| ---------------------------- | ---------- | ------------------------------------------------------------------ |
| `json.stringify(&value:any)` | `str!`     | Codificación de solo lectura en un nuevo texto JSON compacto       |
| `json.parse(text:str)`       | `KuValue!` | Análisis estricto en un valor dinámico con propiedad independiente |
| `json.try_parse(text:str)`   | `KuValue!` | Alias de compatibilidad de `parse`                                 |
| `json.encode(&value:any)`    | `str!`     | Alias de `stringify` incluido en esta versión                      |
| `json.decode(text:str)`      | `KuValue!` | Alias de `parse` incluido en esta versión                          |

`!` indica Result. Usa `?` para propagar errores o `try/catch` para gestionarlos. La codificación no consume ni modifica la entrada.

## Codifica y lee valores

Ejemplo básico completo; más usos en [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.encode(value)?
    println(text)

    decoded = json.decode(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

Las claves de los objetos se ordenan de forma determinista. Salida esperada:

```text
{"count":7,"name":"Ku"}
Ku
8
```

Un `KuValue` analizado no puede participar directamente en aritmética de enteros. Primero conviértelo con `.as_int()?` o `.as_str()?`. Un tipo incompatible devuelve un error. Las claves de objeto ausentes también devuelven errores en vez de valores predeterminados automáticos.

## Datos y límites

Las entradas admitidas incluyen cadenas, booleanos, enteros, números finitos de coma flotante, null, arrays anidados, objetos con claves de cadena y structs representables. No pueden codificarse funciones, Tasks, identificadores de recursos, Results, enums ni ciclos.

| Regla                       | Implementación actual                                                                                    |
| --------------------------- | -------------------------------------------------------------------------------------------------------- |
| Sintaxis JSON               | Estricta; rechaza comentarios, comas finales, contenido adicional al final y Unicode inválido            |
| Entrada / salida            | Cada una limitada a 1.000.000 de bytes                                                                   |
| Límite de profundidad       | La raíz tiene profundidad 0; rechaza `depth > 32`; acepta 33 niveles de contenedores vacíos y rechaza 34 |
| Enteros                     | Con signo de 64 bits; el desbordamiento es un error, sin conversión automática a coma flotante           |
| Coma flotante               | binary64 finito; no conserva la representación original, las etiquetas de flotante ni el cero negativo   |
| Unicode                     | Chino, emojis, escapes, NUL incrustado                                                                   |
| Claves de objeto duplicadas | El análisis conserva el último valor                                                                     |
| Orden de claves             | La codificación ordena por bytes UTF-8                                                                   |

`1.0` puede codificarse como `1`. Si el texto entero de un float supera int64, se añade `.0`: `1e20` pasa a `100000000000000000000.0` y vuelve a decodificarse. Los binary64 finitos no conservan escritura original, etiqueta float ni signo del cero negativo. Esta corrección está incluida.

## Errores

Los errores recuperables utilizan el dominio `json`. Los códigos habituales son `parse_error`, `stringify_error` y `out_of_memory`. El `byte N` de un mensaje de análisis es un desplazamiento de bytes UTF-8 que empieza en cero. Consulta un manejador mínimo de entrada inválida en [errores y resolución de problemas](/es/v0.0.18/docs/guide/errors/).

## Compatibilidad de backends

El intérprete, el ejecutor empaquetado predeterminado y el backend C nativo síncrono tienen implementaciones de JSON. **El subconjunto nativo restringido de Tasks y LLVM no admiten JSON.** Las importaciones del ejecutor predeterminado siguen dependiendo de sus ubicaciones de origen; la compatibilidad con JSON no implica un paquete completamente independiente.

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md#127-json).
