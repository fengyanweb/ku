---
title: Primeros pasos
description: Escribe Hello Ku, comprueba tu código y distingue la interpretación
  de la compilación.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - examples/hello.ku
  - README.md
  - docs/syntax.md
  - docs/v0.0.18.md
slug: es/v0.0.18/docs/getting-started
---

Esta documentación describe el alcance fijado de la versión preliminar experimental v0.0.18. La publicación real y las descargas constan en GitHub Releases. Esta página utiliza sintaxis básica y comandos de CLI ya presentes en v0.0.17. Primero [instala Ku](/es/v0.0.18/docs/install/) y confirma que `ku --version` funciona.

## Escribe tu primer programa

Crea `hello.ku` con el mismo contenido que `examples/hello.ku` del repositorio:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` es el punto de entrada. `println` escribe una línea de texto. Ku no utiliza `let` ni `let mut`.

## Comprueba y ejecuta

Desde el directorio del archivo:

```sh
ku check hello.ku
ku run hello.ku
```

`check` comprueba errores léxicos, sintácticos y semánticos básicos sin ejecutar el programa. La salida estándar esperada al ejecutarlo es:

```text
Hello Ku
```

Si Ku no está en `PATH`, invoca el ejecutable mediante su ruta de instalación completa. En Windows, el ejecutable es `ku.exe`.

## Variables y funciones

Este fragmento de sintaxis básica puede colocarse dentro de `main`:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

La primera asignación declara una variable. Para indicar un tipo explícito se utiliza `name:type = value`. `print` escribe sin añadir un salto de línea; `println` sí lo añade.

## Genera un ejecutable

La página de instalación enumera las herramientas de compilación necesarias. Para el backend predeterminado:

```sh
ku build hello.ku
```

El backend predeterminado empaqueta un ejecutor con intérprete. La compilación nativa en C requiere un compilador de C independiente:

```sh
ku build --backend c hello.ku
```

La salida predeterminada se encuentra en `.ku/build/<profile>/`; utiliza la ruta exacta que indique la CLI. Los dos backends cubren funciones diferentes. Superar `ku check` no demuestra que una compilación nativa vaya a funcionar.

Nota: `ku build --native hello.ku` **sin `-o` solo genera código fuente C**, sin enlazarlo. Para tu primer programa compilado, utiliza el comando `--backend c` anterior.

## Crea un proyecto

Utiliza una plantilla integrada cuando necesites un directorio de paquete:

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

El punto de entrada del paquete se configura en `ku.mod`. `ku create --list` enumera las plantillas disponibles. Actualmente no existen los comandos `ku test` ni `ku fmt`.

Continúa con la [guía del lenguaje](/es/v0.0.18/docs/guide/language/), la [resolución de problemas](/es/v0.0.18/docs/guide/errors/) y la [compatibilidad de backends](/es/v0.0.18/docs/support/). Las salidas mostradas aquí son ejemplos esperados y no sustituyen la verificación de la instalación en tu plataforma.
