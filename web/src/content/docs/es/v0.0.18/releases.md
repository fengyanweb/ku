---
title: 'Notas de versiones'
description: 'Ku 0.0.18 — Notas de versiones'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
slug: es/v0.0.18/releases
---

## v0.0.18

Corrige la evaluación repetida de receptores en cadenas de llamadas, las constantes float de C nativo y `time.date(Time): Date!`, y actualiza la extensión de VS Code.

- JSON: `encode`/`decode` comparten `stringify`/`parse`, incluida la corrección de ida y vuelta de `1e20`.

[Descargar Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Compatibilidad de plataformas y backends](/es/v0.0.18/docs/support/)

## v0.0.17 · Versión preliminar experimental

Cambios principales:

- Los parámetros prestados síncronos de solo lectura utilizan `&name: T`, con argumentos de llamada normales. Los parámetros normales con propiedad conservan la semántica de movimiento.
- El subconjunto del parser autohospedado añade `while` con llaves, con límites de anidación y número de instrucciones.
- Los archivos incluyen las dependencias Rust correspondientes para el ejecutor predeterminado y un paquete TLS del destino. Siguen siendo necesarias las cadenas de herramientas externas apropiadas.

Las descargas cubren Windows x64, Linux x64 y macOS arm64. Los siete recursos de la versión son tres archivos de plataforma, tres manifiestos y `SHA256SUMS`. Consulta la [instalación](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) y las [notas completas de la versión](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Compatibilidad y límites

Ku sigue siendo experimental y no está listo para producción. La versión 0.0.x no promete compatibilidad de código fuente ni de la ABI C/FFI generada. Recompila los artefactos nativos correspondientes al actualizar. Los parámetros async prestados, el autohospedaje completo y async nativo completo quedan fuera de esta versión.

El ejecutor predeterminado solo incorpora el código fuente de entrada; los módulos importados siguen necesitando sus ubicaciones originales. Los clientes de bases de datos son solo nativos. Comprueba los límites de servidor real, TLS y estado de sesión de cada controlador. El código propio de Ku sigue sin tener una concesión de licencia de código abierto.
