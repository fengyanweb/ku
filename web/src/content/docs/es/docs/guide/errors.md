---
title: 'Errores y resolución de problemas'
description: 'Diagnostica errores de instalación, importaciones, propiedad, JSON y capacidades de los backends de Ku.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
    'docs/v0.0.18-r1.md',
  ]
---

Los diagnósticos del compilador pueden cambiar durante el desarrollo experimental. Al informar de un problema, incluye la versión de la CLI, el commit de Git, el backend, el comando completo y un código fuente mínimo.

## Identifica el ejecutable de Ku

```sh
ku --version
ku -h
ku check hello.ku
```

Si no se encuentra `ku`, invoca el ejecutable desde el directorio donde lo extrajiste. En Windows, utiliza `ku.exe`. Si a la compilación predeterminada le faltan Rust o bibliotecas de enlace, revisa los [requisitos de instalación](/es/docs/install/) y conserva la estructura completa del archivo, en lugar de copiar solo la CLI.

## Lee los diagnósticos

La comprobación normal informa del archivo, la ubicación en el código, un fragmento del código y las recomendaciones de corrección disponibles. Las integraciones con herramientas pueden utilizar:

```sh
ku check --json hello.ku
```

La salida está en formato **JSON Lines**, con un diagnóstico por línea. Los campos incluyen `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes` y `helps`. Si todo es correcto, no hay salida; la ausencia de JSON no significa que la comprobación no se haya ejecutado.

| Diagnóstico | Causa habitual                               | Qué hacer                                                                         |
| ----------- | -------------------------------------------- | --------------------------------------------------------------------------------- |
| `E0105`     | Uso de `let`                                 | Declarar la variable mediante su primera asignación                               |
| `E0302`     | Condición de `if` / `while` que no es bool   | Escribir una comparación booleana explícita                                       |
| `E0601`     | Módulo desconocido de la biblioteca estándar | Comprobar la ruta real del módulo, como `std.json`                                |
| `E0605`     | Falta una importación explícita obligatoria  | Importar el módulo, por ejemplo `import fs from "std.fs"`                         |
| `E0901`     | Lectura de un valor movido                   | Clonar explícitamente un valor independiente o usar el préstamo síncrono admitido |
| `E0913`     | Parámetro async prestado                     | Usar un parámetro con propiedad; clonar en el llamador si se necesita el original |
| `E0918`     | Escribir `&value` en la llamada              | Pasar un argumento normal; la firma declara el préstamo                           |

Este es un resumen de los diagnósticos registrados, no una lista de todos los errores. El registro vigente y autorizado es [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/error.rs).

## Los errores de ejecución tienen un contrato independiente

Los valores `domain/code` de Error en tiempo de ejecución son independientes de los códigos `E` del compilador. Un JSON inválido puede devolver `json/parse_error`; la ausencia de una clave de objeto puede devolver `object/missing_key`. Usa `?` después de una llamada que pueda fallar o lee sus campos en `try/catch`.

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

Este programa didáctico provoca deliberadamente un error de análisis recuperable. El valor `byte N` indicado es un desplazamiento de bytes UTF-8 que empieza en cero. No es un recuento de caracteres en textos chinos ni emojis.

## Se rechaza una compilación nativa

Superar `ku check` solo acredita que la comprobación del frontend fue correcta. Los backends C nativo y LLVM tienen sus propios límites. Consulta la [matriz de compatibilidad](/es/docs/support/) cuando se rechace explícitamente una construcción. Aumentar un tiempo de espera o tener éxito con otro backend no verifica la ruta no admitida.

`ku build --native file.ku` sin `-o` solo escribe C. Para compilar y enlazar un ejecutable, utiliza `ku build --backend c file.ku` e instala la cadena de herramientas de C para el destino.

Los errores de base de datos `execution_unknown` y `execution_completed_without_result` no deben provocar una repetición automática. Conserva las pruebas de la solicitud y de la operación de negocio y determina primero el estado real de ejecución.

Para más detalles, consulta la [guía de diagnósticos del repositorio](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/diagnostics.md).

Los errores JSON conservan domain/code entre backends, pero message puede diferir. ku run y el runner pueden añadir json parse failed:, mientras C nativo no. Compara domain/code, no el texto completo.
