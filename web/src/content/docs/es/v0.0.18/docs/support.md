---
title: 'Compatibilidad de plataformas y backends'
description: 'Distingue las plataformas publicadas de Ku, intérprete, ejecutor, C nativo, subconjunto de Tasks y compatibilidad LLVM.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
    'docs/v0.0.18-r1.md',
  ]
slug: es/v0.0.18/docs/support
---

## Plataformas de CLI publicadas

La versión preliminar experimental v0.0.18 cubre Windows x64 (MSVC), Linux x64 (GNU) y macOS arm64 (Apple Silicon). Cada plataforma tiene su propio archivo. No existe un único ejecutable para los tres sistemas. No se verificaron recursos de otras plataformas; consulta la [instalación](/es/v0.0.18/docs/install/).

## Rutas de ejecución

| Capacidad                                                             | Intérprete `ku run`                  | Ejecutor predeterminado `ku build`        | C nativo síncrono                                                  | Task nativo (subconjunto)                         | Prototipo de texto LLVM     |
| --------------------------------------------------------------------- | ------------------------------------ | ----------------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------- | --------------------------- |
| Lenguaje básico, funciones, flujo de control                          | Implementado                         | Ruta del intérprete                       | Subconjunto implementado                                           | Subconjunto tipado restringido                    | Prototipo limitado          |
| JSON                                                                  | Admitido                             | Admitido                                  | Admitido                                                           | No admitido                                       | No admitido                 |
| Grafo local completo de importaciones independiente del código fuente | Carga código fuente                  | No admitido; solo se incorpora la entrada | Se expande durante la generación                                   | Se expande dentro del subconjunto admitido        | Sin promesa de despliegue   |
| async / await                                                         | Implementado con límites de recursos | Ruta del intérprete                       | Subconjunto específico o rechazo                                   | Parcial, sin async completo                       | Sin compatibilidad completa |
| Clientes PostgreSQL / Redis / MySQL                                   | Sin acceso a bases de datos          | Sin acceso interpretado a bases de datos  | Solo nativos                                                       | Sin espera genérica de Tasks                      | No admitidos                |
| Cliente TCP / TLS                                                     | Sin ruta nativa equivalente          | Sin ruta nativa equivalente               | Implementación C generada; TLS requiere un paquete correspondiente | Sin afirmación de espera de red genérica completa | No admitido                 |

Las compilaciones predeterminadas requieren la cadena de herramientas de Rust correspondiente y el enlazador. C nativo requiere una cadena de herramientas de C para el destino. Superar `ku check` no demuestra que ninguna de las dos compilaciones funcione. Un comando LLVM no implica un backend completo de publicación.

## El subconjunto nativo de Tasks

Esta versión incluye la ruta C nativa restringida `async fn main(): null!`: primitivas enumeradas, cadenas, arrays planos, Results de un nivel, inicio/movimiento/await de Task, parte del control de flujo y operaciones acotadas de archivos/directorios en segundo plano. TCP/TLS local usa un pool bloqueante fijo. No equivale a readiness de red general ni a M:N completo; consulta la referencia de concurrencia de esta versión.

Siguen pendientes las esperas de red genéricas de Tasks, la ejecución M:N completa, las combinaciones arbitrarias de heap dinámico, todas las combinaciones de capturas/funciones async y las pruebas prolongadas completas. Consulta el subconjunto exacto aceptado y rechazado en el [contrato de concurrencia](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).

## Límites de bases de datos y red

Las APIs de base de datos utilizan `module.client(config)?`, seguido de métodos del receptor, con un pool limitado dentro del cliente. La validación con servidores reales es independiente para cada controlador, sistema y commit. Una ABI simulada o un enlace correcto no sustituyen una consulta real.

Redis utiliza TCP sin cifrar. MySQL carece de una verificación uniforme de certificados que cierre la conexión ante errores entre Oracle/MariaDB. Por eso, las conexiones directas deben usar loopback, redes privadas controladas o un túnel TLS verificado. PostgreSQL remoto requiere `sslmode=verify-full` explícito y certificados raíz de confianza. El TLS nativo de `std.net` no implica que HTTP, Redis o el intérprete reutilicen esa ruta.

## Autohospedaje y compatibilidad

El analizador léxico y el parser autohospedados de Ku tienen subconjuntos limitados y comprobaciones diferenciales. Un parser, comprobador, IR y backend autohospedados completos siguen sin terminar; Rust continúa proporcionando la cadena de herramientas de arranque. La versión 0.0.x no promete compatibilidad de código fuente ni de la ABI C/FFI generada.

## Alcance y límites de v0.0.18

- JSON: `encode`/`decode` comparten `stringify`/`parse`, incluida la corrección de ida y vuelta de `1e20`.
- Task nativo mantiene el subconjunto acotado; correcciones de admisión y limpieza al cerrar no equivalen a M:N completo ni a suspensión de red general.
- HTTP ofrece readiness de sockets, colas, respuestas y keep-alive acotados y diagnósticos acotados opcionales; no implica aceptación de todas las cargas.
- `ku supervise` controla workers de programas completos, admisión, bytes de colas, salida y limpieza; no hay un worker por petición o Task ni se aplica automáticamente a `ku run`.

**Problema HTTP conocido:** Pueden producirse errores HTTP 504 intermitentes. La causa aún no está confirmada.

### Límites de memoria de procesos

| Límites de memoria de procesos | `ku supervise`                                                                                                                                                                                                                                            |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux                          | Memoria contabilizada por cgroup v2; requiere raíz de dominio delegada y escribible con `memory`, `pids`, `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` y `cgroup.kill`. La falta de permisos o controles impide iniciar la aplicación. |
| Windows                        | Job Objects anidados limitan workers y grupo según memoria virtual comprometida, no RSS. Se comprueba el compromiso inicial antes de liberar el hilo suspendido; la política del host puede rechazar la asignación al Job.                                |
| macOS                          | Este aislamiento no está admitido de forma nativa y se rechaza antes de ejecutar. Usa un host o VM Linux configurado adecuadamente; no hay transferencia automática a una VM.                                                                             |

El dominio del worker contabiliza asignaciones Ku y de bibliotecas externas. cgroup Linux y compromiso Windows son medidas diferentes. `memory.max` puede permitir excesos transitorios; no se garantiza un techo absoluto de toda la memoria física en todo instante.

El coordinador no tiene límite estricto de memoria del SO; reservas y contabilidad de admisión no son RSS preciso. No se mide exactamente todo el heap de cada Task ni se atribuyen con precisión pools, cachés y bibliotecas. Bases externas, host/VM, compiladores y `ku run` ordinario no están limitados automáticamente por el presupuesto del worker.

Si el SO termina un worker, se informa el fallo y la reserva solo vuelve tras confirmar la limpieza. Algunas asignaciones fallan anticipadamente, pero el agotamiento de bibliotecas externas no siempre produce Result ordinario. **Las operaciones de base de datos nunca se repiten automáticamente**; salir del worker no demuestra rollback y sus efectos pueden ser desconocidos.

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
