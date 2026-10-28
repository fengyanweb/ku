---
title: 'Proyecto y contribuciones'
description: 'Estado del proyecto Ku, código público, límites de licencia e informes de problemas reproducibles.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

La documentación actual corresponde a Ku 0.0.19, una versión preliminar experimental con la misma versión de CLI y extensión de VS Code. La disponibilidad, los archivos y las sumas de comprobación dependen del GitHub Release real.

**Ku programming language / Ku 编程语言** · CLI: `ku`.

## Código fuente y licencia

[GitHub](https://github.com/fengyanweb/ku) · [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)

Ku y la extensión de VS Code usan Apache License 2.0. Consulta [LICENSE](https://github.com/fengyanweb/ku/blob/main/LICENSE) y [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE). Los componentes de terceros conservan sus propios derechos de autor y licencias.

## Informa de un problema

Utiliza los [Issues](https://github.com/fengyanweb/ku/issues) del repositorio para informes reproducibles. Incluye:

- Versión de Ku, commit de Git si utilizas código de desarrollo y sistema / arquitectura de destino.
- Comando completo, backend y código `.ku` mínimo.
- Diagnóstico completo o Error estructurado, salida esperada y salida real.
- Versiones de las herramientas Rust / C para problemas de compilación y si intervienen importaciones locales.

No envíes tokens, contraseñas, cadenas de conexión reales ni registros con datos empresariales sensibles. Los informes de rendimiento deben incluir entorno, carga de trabajo, método de medición y resultados en bruto. Una muestra exitosa no acredita el rendimiento en producción.

## Antes de contribuir

El [contrato de sintaxis](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) y el [contrato semántico](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) definen el comportamiento del lenguaje. Revisa conjuntamente CI, implementación y contratos. Un objetivo del registro de trabajo no es una función implementada.

Limita los cambios y explica reproducción, causa, alcance y validación. Revisa el contenido del sitio junto al código correspondiente. Conserva las pruebas existentes, los cambios del árbol de trabajo y las evidencias únicas. El mantenedor decide la aceptación de contribuciones y los acuerdos de licencia; este sitio no inventa un acuerdo de contribución.

Este sitio es estático. Sus textos, navegación y código no requieren un backend empresarial. La búsqueda carga un índice Pagefind en el navegador; el sitio no ofrece ejecución remota de código Ku.

Continúa con [primeros pasos](/es/docs/getting-started/), [límites de compatibilidad](/es/docs/support/) y [notas de versiones](/es/releases/).
