---
title: Proyecto y contribuciones
description: Estado del proyecto Ku, código público, límites de licencia e
  informes de problemas reproducibles.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - README.md
  - Cargo.toml
  - editors/vscode-ku/package.json
  - docs/v0.0.18.md
  - docs/syntax.md
slug: es/v0.0.18/docs/project
---

Esta documentación describe el alcance fijado de la versión preliminar experimental v0.0.18. La publicación real y las descargas constan en GitHub Releases. El proyecto se llama **Ku programming language / Ku 编程语言**, y su CLI se llama `ku`. Ku es un pequeño lenguaje de programación e intérprete en desarrollo. Su versión pública actual es la preliminar experimental v0.0.18.

## Código fuente y licencia

Consulta el [código en GitHub](https://github.com/fengyanweb/ku) y la [página de la versión](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). El código es visible públicamente, pero el código propio de Ku **no tiene una concesión de licencia de código abierto**. La extensión del editor está marcada como `UNLICENSED`. Publicar código o binarios no concede automáticamente una licencia de código abierto.

Los avisos de terceros incluidos en los archivos de versiones solo se aplican a sus respectivos componentes. No conceden permisos de uso, modificación ni redistribución de Ku. Cualquier autorización necesaria debe proceder expresamente del mantenedor; este sitio web no crea nuevos compromisos de licencia.

## Informa de un problema

Utiliza los [Issues](https://github.com/fengyanweb/ku/issues) del repositorio para informes reproducibles. Incluye:

* Versión de Ku, commit de Git si utilizas código de desarrollo y sistema / arquitectura de destino.
* Comando completo, backend y código `.ku` mínimo.
* Diagnóstico completo o Error estructurado, salida esperada y salida real.
* Versiones de las herramientas Rust / C para problemas de compilación y si intervienen importaciones locales.

No envíes tokens, contraseñas, cadenas de conexión reales ni registros con datos empresariales sensibles. Los informes de rendimiento deben incluir entorno, carga de trabajo, método de medición y resultados en bruto. Una muestra exitosa no acredita el rendimiento en producción.

## Antes de contribuir

El [contrato de sintaxis](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) y el [contrato semántico](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) definen el comportamiento del lenguaje. Revisa conjuntamente CI, implementación y contratos. Un objetivo del registro de trabajo no es una función implementada.

Limita los cambios y explica reproducción, causa, alcance y validación. Revisa el contenido del sitio junto al código correspondiente. Conserva las pruebas existentes, los cambios del árbol de trabajo y las evidencias únicas. El mantenedor decide la aceptación de contribuciones y los acuerdos de licencia; este sitio no inventa un acuerdo de contribución.

Este sitio es estático. Sus textos, navegación y código no requieren un backend empresarial. La búsqueda carga un índice Pagefind en el navegador; el sitio no ofrece ejecución remota de código Ku.

Continúa con [primeros pasos](/es/v0.0.18/docs/getting-started/), [límites de compatibilidad](/es/v0.0.18/docs/support/) y [notas de versiones](/es/v0.0.18/releases/).
