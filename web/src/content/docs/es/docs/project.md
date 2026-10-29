---
title: 'Proyecto y contribuciones'
description: 'Estado del proyecto Ku, código público, límites de licencia e informes de problemas reproducibles.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'LICENSE',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Ku es un lenguaje de programación y una cadena de herramientas: intérprete, runner ejecutable predeterminado y compilación C nativa de un subconjunto admitido.

## Código fuente y licencia

Ku y su extensión para VS Code se distribuyen bajo **Apache-2.0**. Consulta el [código fuente](https://github.com/fengyanweb/ku), la [versión 0.0.19](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) y el [texto de la licencia](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE).

Los componentes de terceros conservan sus propias licencias. Los paquetes incluyen los textos y avisos correspondientes.

## Informa de un problema

Utiliza los [Issues](https://github.com/fengyanweb/ku/issues) del repositorio para informes reproducibles. Incluye:

Comentarios por correo: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Versión de Ku, commit de Git si utilizas código de desarrollo y sistema / arquitectura de destino.
- Comando completo, backend y código `.ku` mínimo.
- Diagnóstico completo o Error estructurado, salida esperada y salida real.
- Versiones de las herramientas Rust / C para problemas de compilación y si intervienen importaciones locales.

No envíes tokens, contraseñas, cadenas de conexión reales ni registros con datos empresariales sensibles. Los informes de rendimiento deben incluir entorno, carga de trabajo, método de medición y resultados en bruto. Una muestra exitosa no acredita el rendimiento en producción.

## Antes de contribuir

Mantén los cambios enfocados e incluye una reproducción mínima, el alcance y las comprobaciones pertinentes. El código, la documentación y los ejemplos deben corresponder a la misma versión. Los responsables revisan las contribuciones antes de integrarlas.

El sitio no ofrece ejecución de código Ku en línea.

Continúa con [primeros pasos](/es/docs/getting-started/), [límites de compatibilidad](/es/docs/support/) y [notas de versiones](/es/releases/).
