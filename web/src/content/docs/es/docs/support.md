---
title: 'Compatibilidad de plataformas y backends'
description: 'Distingue las plataformas publicadas de Ku, intérprete, ejecutor, C nativo, subconjunto de Tasks y compatibilidad LLVM.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.19.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
---

Esta documentación describe el alcance fijado de la versión preliminar experimental v0.0.18. La publicación real y las descargas constan en GitHub Releases.

## Plataformas de CLI publicadas

La versión preliminar experimental v0.0.18 cubre Windows x64 (MSVC), Linux x64 (GNU) y macOS arm64 (Apple Silicon). Cada plataforma tiene su propio archivo. No existe un único ejecutable para los tres sistemas. No se verificaron recursos de otras plataformas; consulta la [instalación](/es/docs/install/).

## Rutas de ejecución

| Capacidad                                                             | Intérprete `ku run`                  | Ejecutor predeterminado `ku build`        | C nativo síncrono                                                  | Tasks nativos en desarrollo                       | Prototipo de texto LLVM     |
| --------------------------------------------------------------------- | ------------------------------------ | ----------------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------- | --------------------------- |
| Lenguaje básico, funciones, flujo de control                          | Implementado                         | Ruta del intérprete                       | Subconjunto implementado                                           | Subconjunto tipado restringido                    | Prototipo limitado          |
| JSON                                                                  | Admitido                             | Admitido                                  | Admitido                                                           | No admitido                                       | No admitido                 |
| Grafo local completo de importaciones independiente del código fuente | Carga código fuente                  | No admitido; solo se incorpora la entrada | Se expande durante la generación                                   | Se expande dentro del subconjunto admitido        | Sin promesa de despliegue   |
| async / await                                                         | Implementado con límites de recursos | Ruta del intérprete                       | Subconjunto específico o rechazo                                   | Parcial, sin async completo                       | Sin compatibilidad completa |
| Clientes PostgreSQL / Redis / MySQL                                   | Sin acceso a bases de datos          | Sin acceso interpretado a bases de datos  | Solo nativos                                                       | Sin espera genérica de Tasks                      | No admitidos                |
| Cliente TCP / TLS                                                     | Sin ruta nativa equivalente          | Sin ruta nativa equivalente               | Implementación C generada; TLS requiere un paquete correspondiente | Sin afirmación de espera de red genérica completa | No admitido                 |

Las compilaciones predeterminadas requieren la cadena de herramientas de Rust correspondiente y el enlazador. C nativo requiere una cadena de herramientas de C para el destino. Superar `ku check` no demuestra que ninguna de las dos compilaciones funcione. Un comando LLVM no implica un backend completo de publicación.

## El subconjunto nativo de Tasks

La rama de desarrollo actual tiene una ruta de código restringida para `async fn main(): null!`. Cubre los tipos primitivos enumerados expresamente, cadenas, arrays planos, Results de una sola capa, inicio/movimiento/await de Tasks, parte del flujo de control y operaciones limitadas de archivos y directorios en segundo plano. No es un backend async completo en el paquete publicado v0.0.18.

Siguen pendientes las esperas de red genéricas de Tasks, la ejecución M:N completa, las combinaciones arbitrarias de heap dinámico, todas las combinaciones de capturas/funciones async y las pruebas prolongadas completas. Consulta el subconjunto exacto aceptado y rechazado en el [contrato de concurrencia](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/concurrency.md).

## Límites de bases de datos y red

Las APIs de base de datos utilizan `module.client(config)?`, seguido de métodos del receptor, con un pool limitado dentro del cliente. La validación con servidores reales es independiente para cada controlador, sistema y commit. Una ABI simulada o un enlace correcto no sustituyen una consulta real.

Redis utiliza TCP sin cifrar. MySQL carece de una verificación uniforme de certificados que cierre la conexión ante errores entre Oracle/MariaDB. Por eso, las conexiones directas deben usar loopback, redes privadas controladas o un túnel TLS verificado. PostgreSQL remoto requiere `sslmode=verify-full` explícito y certificados raíz de confianza. El TLS nativo de `std.net` no implica que HTTP, Redis o el intérprete reutilicen esa ruta.

## Autohospedaje y compatibilidad

El analizador léxico y el parser autohospedados de Ku tienen subconjuntos limitados y comprobaciones diferenciales. Un parser, comprobador, IR y backend autohospedados completos siguen sin terminar; Rust continúa proporcionando la cadena de herramientas de arranque. La versión 0.0.x no promete compatibilidad de código fuente ni de la ABI C/FFI generada.

Los registros de trabajo y los resultados anteriores de CI solo acreditan su alcance y commit declarados. Usa las [notas de versiones](/es/releases/) para separar los cambios publicados del trabajo de desarrollo. Este sitio no informa como superadas todas las comprobaciones de CI de la rama actual, las cargas de producción ni las validaciones de bases de datos.

> Las novedades de v0.0.18 aparecen a continuación en inglés; se conserva el resto de la traducción. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions.

### Process memory boundary

| Host    | `ku supervise` guarantee and prerequisite                                                                                                                                                                                                                                                                   |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment.                                                          |
| macOS   | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge.                                                                                                           |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.
