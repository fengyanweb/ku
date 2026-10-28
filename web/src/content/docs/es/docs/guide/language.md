---
title: 'Guía del lenguaje'
description: 'Aprende las variables, funciones, flujo de control, propiedad, módulos y errores recuperables de Ku.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
  ]
---

Esta documentación describe el alcance fijado de la versión preliminar experimental v0.0.18. La publicación real y las descargas constan en GitHub Releases. Esta guía presenta la sintaxis esencial. El [contrato de sintaxis](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) y el [contrato semántico](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) del repositorio siguen siendo las referencias autorizadas. Son fragmentos didácticos; consulta las diferencias en la [compatibilidad de backends](/es/docs/support/).

## Variables y tipos

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

La primera asignación declara una variable. Los tipos primitivos son `int`, `float`, `bool`, `str` y `null`. Los enteros tienen signo y 64 bits; los valores de coma flotante utilizan binary64. Los enteros no se convierten implícitamente en condiciones booleanas.

## Funciones y flujo de control

Este ejemplo básico completo procede de `examples/function.ku`:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Salida esperada: `30`. Este fragmento de flujo de control puede colocarse dentro de `main`:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` itera desde `0` hasta `4`. `if` y `while` requieren condiciones de tipo `bool`. Usa llaves para varias instrucciones. Las expresiones de ramificación utilizan `match`; no existe `switch`.

## Propiedad y préstamos de solo lectura

`int`, `float`, `bool` y `null` son valores Copy. Las cadenas, los arrays, objetos, structs, enums, Results y valores de función son Owned. Las asignaciones y los argumentos normales mueven los valores con propiedad de forma predeterminada. Utiliza `.clone()` cuando necesites un valor independiente. Los Tasks no pueden clonarse.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Una función síncrona puede declarar un parámetro prestado de solo lectura:

```ku
fn length(&text: str): int {
    return text.len()
}
```

Llámala como `length(value)`, sin `&` en el argumento. Los datos prestados no pueden modificarse, moverse fuera ni escapar de la llamada. Las combinaciones como los parámetros async prestados siguen sin admitirse. Los préstamos de solo lectura no garantizan que cualquier función sea pura.

## Structs, objetos y módulos

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Los objetos dinámicos utilizan `{ name: "Ku" }`. Un acceso normal a un índice de objeto ausente falla. `value["name"]?` realiza la búsqueda mediante Result y devuelve un `KuValue` en caso de éxito. Usa `.as_str()?` o `.as_int()?` para obtener un tipo concreto. Una clave ausente no se transforma silenciosamente en `null`.

Las importaciones de la biblioteca estándar utilizan rutas con puntos, como `import json from "std.json"`. Los módulos locales pueden usar `import math from "./math"`. Las exportaciones de nivel superior definidas por el usuario empiezan por una letra ASCII mayúscula; los nombres en minúsculas permanecen privados en el archivo.

## Errores recuperables

El tipo de retorno `T!` puede devolver T o Error. `?` propaga un error recuperable; `fail` lo crea:

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    try {
        println(read_name()?)
    } catch (error) {
        println(error.message)
    }
}
```

Error tiene los campos `domain`, `code` y `message`. Un `catch` normal no captura `panic`, cancelaciones de Tasks ni tiempos de espera agotados. La limpieza por cancelación en finally comparte un presupuesto de plazo absoluto; no garantiza completar cualquier limpieza del usuario.

## Límites de concurrencia

El código de aplicación obtiene un identificador Task de un solo uso llamando a una `async fn` y después utiliza `await`. No existen APIs de aplicación llamadas `task.spawn`, `Task.new` ni `thread.spawn`. El intérprete y el subconjunto nativo de Tasks tienen una cobertura diferente. Un ejemplo que funciona en un backend no demuestra compatibilidad completa en otro. Consulta la [compatibilidad de plataformas y backends](/es/docs/support/).
