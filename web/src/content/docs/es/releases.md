---
title: 'Notas de versiones'
description: 'Ku v0.0.19 experimental prerelease scope, downloads and known limitations.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/v0.0.19.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
  ]
---

## 0.0.19

La documentación actual corresponde a Ku 0.0.19, una versión preliminar experimental con la misma versión de CLI y extensión de VS Code. La disponibilidad, los archivos y las sumas de comprobación dependen del GitHub Release real.

[Release · SHA256SUMS](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [Installation](/es/docs/install/) · [Release notes (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.en.md) · [Known issues (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.en.md)

## v0.0.18 · Histórico

Esta documentación describe el alcance fijado de la versión preliminar experimental v0.0.18. La publicación real y las descargas constan en GitHub Releases.

> Las novedades de v0.0.18 aparecen a continuación en inglés; se conserva el resto de la traducción. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

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

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.

## v0.0.17 · Versión preliminar experimental

Publicada el **2026-10-18 (America/Los_Angeles)**, o `2026-10-19T03:51:10Z` en UTC. La versión tiene `prerelease=true` y `draft=false`. Actualmente no se ha verificado ninguna versión estable.

Cambios principales:

- Los parámetros prestados síncronos de solo lectura utilizan `&name: T`, con argumentos de llamada normales. Los parámetros normales con propiedad conservan la semántica de movimiento.
- El subconjunto del parser autohospedado añade `while` con llaves, con límites de anidación y número de instrucciones.
- Las pruebas entre plataformas corrigen las comprobaciones de desconexión de stdout. Esto no añade una API de negocio.
- Los archivos incluyen las dependencias Rust correspondientes para el ejecutor predeterminado y un paquete TLS del destino. Siguen siendo necesarias las cadenas de herramientas externas apropiadas.

Las descargas cubren Windows x64, Linux x64 y macOS arm64. Los siete recursos de la versión son tres archivos de plataforma, tres manifiestos y `SHA256SUMS`. Consulta la [instalación](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) y las [notas completas de la versión](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Compatibilidad y límites

Ku sigue siendo experimental y no está listo para producción. La versión 0.0.x no promete compatibilidad de código fuente ni de la ABI C/FFI generada. Recompila los artefactos nativos correspondientes al actualizar. Los parámetros async prestados, el autohospedaje completo y async nativo completo quedan fuera de esta versión.

El ejecutor predeterminado solo incorpora el código fuente de entrada; los módulos importados siguen necesitando sus ubicaciones originales. Los clientes de bases de datos son solo nativos. Comprueba los límites de servidor real, TLS y estado de sesión de cada controlador. El código propio de Ku sigue sin tener una concesión de licencia de código abierto.
