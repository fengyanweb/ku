---
title: 'Proyecto y contribuciones'
description: 'Estado del proyecto Ku, código público, límites de licencia e informes de problemas reproducibles.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

Esta documentación describe el alcance fijado de la versión preliminar experimental v0.0.18. La publicación real y las descargas constan en GitHub Releases. El proyecto se llama **Ku programming language / Ku 编程语言**, y su CLI se llama `ku`. Ku es un pequeño lenguaje de programación e intérprete en desarrollo. Su versión pública actual es la preliminar experimental v0.0.18.

## Código fuente y licencia

El código propio de Ku en las instantáneas que incluyen el archivo de licencia, así como la extensión actual del editor, se distribuyen bajo [Apache License 2.0](https://github.com/fengyanweb/ku/blob/main/LICENSE) (`Apache-2.0`); consulta [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE) para la atribución. Consulta el [código en GitHub](https://github.com/fengyanweb/ku) y la [página de la versión](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Los componentes de terceros conservan sus propias licencias.

Los archivos de v0.0.18-r1 y versiones anteriores conservan los materiales de licencia originales que los acompañan; este cambio de licencia del código fuente no vuelve a empaquetarlos. Usar el compilador de Ku no exige publicar el código fuente de tus propios programas. Al distribuir programas que contienen código del entorno de ejecución de Ku, cumple los requisitos de licencia y atribución aplicables, incluidos los avisos de terceros.

## Informa de un problema

Utiliza los [Issues](https://github.com/fengyanweb/ku/issues) del repositorio para informes reproducibles. Incluye:

- Versión de Ku, commit de Git si utilizas código de desarrollo y sistema / arquitectura de destino.
- Comando completo, backend y código `.ku` mínimo.
- Diagnóstico completo o Error estructurado, salida esperada y salida real.
- Versiones de las herramientas Rust / C para problemas de compilación y si intervienen importaciones locales.

No envíes tokens, contraseñas, cadenas de conexión reales ni registros con datos empresariales sensibles. Los informes de rendimiento deben incluir entorno, carga de trabajo, método de medición y resultados en bruto. Una muestra exitosa no acredita el rendimiento en producción.

## Antes de contribuir

El [contrato de sintaxis](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) y el [contrato semántico](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) definen el comportamiento del lenguaje. Revisa conjuntamente CI, implementación y contratos. Un objetivo del registro de trabajo no es una función implementada.

Limita los cambios y explica reproducción, causa, alcance y validación. Revisa el contenido del sitio junto al código correspondiente. Conserva las pruebas existentes, los cambios del árbol de trabajo y las evidencias únicas. El mantenedor decide la aceptación de contribuciones y los acuerdos de licencia; este sitio no inventa un acuerdo de contribución.

Este sitio es estático. Sus textos, navegación y código no requieren un backend empresarial. La búsqueda carga un índice Pagefind en el navegador; el sitio no ofrece ejecución remota de código Ku.

Continúa con [primeros pasos](/es/docs/getting-started/), [límites de compatibilidad](/es/docs/support/) y [notas de versiones](/es/releases/).
