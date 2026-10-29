---
title: 'Compatibilidad de plataformas y backends'
description: 'Distingue las plataformas publicadas de Ku, intérprete, ejecutor, C nativo, subconjunto de Tasks y compatibilidad LLVM.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Descargar Ku 0.0.19

Ku 0.0.19 es una versión preliminar experimental. Hay paquetes para macOS ARM64 (Apple Silicon), Windows x64 y Linux x64 (GNU).

[Paquetes e instrucciones de instalación](/es/docs/install/)

## Formas de ejecución

- **`ku run`**: Intérprete para el lenguaje, JSON general y manejadores HTTP síncronos; sin clientes de bases de datos.
- **`ku build`**: El runner predeterminado incorpora el archivo de entrada; las importaciones locales todavía necesitan sus archivos fuente. Requiere Rust **1.89.0** y un enlazador del sistema.
- **`ku build --backend c`**: Admite JSON síncrono, HTTP con `http.json` y clientes nativos de PostgreSQL, Redis y MySQL. Requiere un compilador C, un enlazador y bibliotecas cliente compatibles con el destino. Las operaciones de base de datos no se reintentan automáticamente.
- **Task**: La ruta Task nativa admite un subconjunto tipado con inicio/`await`, temporizadores, operaciones de E/S seleccionadas, manejadores HTTP async con nombre y determinadas consultas PG. En esa ruta, JSON solo puede codificar objetos literales de valores escalares; no ofrece decodificación general. El M:N completo y la cobertura general de E/S aún no están terminados.
- **LLVM**: El backend de texto LLVM sigue siendo un prototipo limitado.

## Problemas conocidos

Las comprobaciones de publicación y de paquetes de las tres plataformas han pasado. Esto no constituye una aprobación general para producción.

- **HTTP 504:** ocurrió en el pasado, pero no se reprodujo en la última revisión corta; la causa sigue sin confirmar.
- **Picos de latencia durante la cancelación:** en la última revisión de 30 segundos, las 60.000 solicitudes cortas fueron correctas. El p99 fue de **1.557 ms** y el máximo de **98.809 ms**; algunas solicitudes todavía superaron los 20 ms.

La validación formal de estabilidad de 30 minutos aún no ha pasado. Las mediciones cortas no la sustituyen.

Referencia completa: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

Informar de problemas: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
