---
title: 'Instalación y descargas'
description: 'Descarga la versión preliminar experimental Ku 0.0.18 y conoce los requisitos de plataforma y compilación.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

> Las novedades de v0.0.18 aparecen a continuación en inglés; se conserva el resto de la traducción. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## Elige tu plataforma

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| Sistema | Arquitectura          | Archivo                                      |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

No se han verificado archivos para otras plataformas. La compatibilidad con compilación cruzada no significa que todos los sistemas operativos o arquitecturas tengan un paquete publicado.

## Extrae y verifica

1. Verifica el archivo con `SHA256SUMS` de la versión. Un `.manifest.json` independiente describe la integridad del archivo y de su contenido. Las sumas de comprobación verifican integridad; no son firmas del editor.
2. Extrae el **archivo completo** en un directorio vacío y lee `README-INSTALL.md`. Conserva la estructura relativa de `ku`, `libku.rlib`, `deps/` y `native-tls/`.
3. Comprueba la versión desde ese directorio. En macOS / Linux:

```sh
./ku --version
```

En Windows PowerShell:

```powershell
.\ku.exe --version
```

La versión indicada debería ser `0.0.18`. Si quieres, añade este directorio a `PATH` y ejecuta `ku --version`. Instala el archivo incluido `ku-language-0.0.18.vsix` para la integración con VS Code.

## Requisitos de ejecución y compilación

| Operación                               | Requisitos adicionales                                                                                  |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | CLI de Ku para la plataforma correspondiente                                                            |
| `ku build hello.ku` predeterminado      | Rust **1.89.0** para el destino y su enlazador de sistema; conservar las dependencias de Rust incluidas |
| `ku build --backend c hello.ku`         | Compilador y enlazador de C funcionales para el destino                                                 |
| Programa nativo de base de datos        | Bibliotecas dinámicas externas del cliente de base de datos correspondientes; no están incluidas        |

El ejecutor predeterminado solo incorpora el código fuente de entrada. Los módulos importados siguen cargándose desde sus ubicaciones originales. El despliegue de un grafo completo de importaciones locales sin depender del código fuente requiere compilación nativa en C y sigue sujeto a los [límites de compatibilidad de backends](/es/docs/support/).

## Prueba el código de desarrollo

Desde una copia local del repositorio:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Desinstala

En una instalación desde un archivo, elimina el directorio extraído, deshaz las entradas de `PATH` que hayas añadido y desinstala la extensión de VS Code. Antes, conserva los archivos fuente y la configuración que hayas creado allí. No se ha verificado un instalador independiente de un clic, una instalación mediante gestor de paquetes ni un comando `ku uninstall`.

Siguiente paso: [ejecuta tu primer programa](/es/docs/getting-started/). Fuente del contrato de instalación: [archivos de versiones](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/es/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
