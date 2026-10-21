---
title: 'Поддержка платформ и бэкендов'
description: 'Различия платформ выпуска Ku, интерпретатора, средства запуска, нативного C, подмножества Task и LLVM.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
---

Документация описывает зафиксированный объём экспериментального предварительного выпуска v0.0.18. Фактическая публикация и файлы загрузки указаны в GitHub Releases.

## Опубликованные платформы CLI

Экспериментальная предварительная версия v0.0.18 охватывает Windows x64 (MSVC), Linux x64 (GNU) и macOS arm64 (Apple Silicon). Для каждой платформы есть свой архив. Единого исполняемого файла для трёх систем нет. Ресурсы для других платформ не проверялись; см. [установку](/ru/docs/install/).

## Пути выполнения

| Возможность                                        | Интерпретатор `ku run`              | Средство запуска `ku build` по умолчанию           | Синхронный нативный C                                     | Нативные Task в разработке                            | Текстовый прототип LLVM       |
| -------------------------------------------------- | ----------------------------------- | -------------------------------------------------- | --------------------------------------------------------- | ----------------------------------------------------- | ----------------------------- |
| Базовый язык, функции, управление потоком          | Реализовано                         | Путь интерпретатора                                | Реализованное подмножество                                | Ограниченное типизированное подмножество              | Ограниченный прототип         |
| JSON                                               | Поддерживается                      | Поддерживается                                     | Поддерживается                                            | Не поддерживается                                     | Не поддерживается             |
| Полный граф локальных импортов без исходных файлов | Загружает исходники                 | Не поддерживается; встраивается только точка входа | Разворачивается при генерации                             | Разворачивается в поддерживаемом подмножестве         | Нет обещаний по развёртыванию |
| async / await                                      | Реализовано с пределами ресурсов    | Путь интерпретатора                                | Отдельное подмножество либо отклонение                    | Частично, не полный async                             | Полной поддержки нет          |
| Клиенты PostgreSQL / Redis / MySQL                 | Нет доступа к БД                    | Нет интерпретируемого доступа к БД                 | Только нативно                                            | Не универсальное ожидание Task                        | Не поддерживается             |
| Клиент TCP / TLS                                   | Нет соответствующего нативного пути | Нет соответствующего нативного пути                | Генерируемая реализация C; TLS требует подходящего пакета | Нет заявления о полном универсальном сетевом ожидании | Не поддерживается             |

Обычная сборка требует подходящих Rust и компоновщика. Нативный C требует инструментов C целевой платформы. Успешный `ku check` не доказывает работоспособность ни одной сборки. Наличие команды LLVM не означает полноценный бэкенд выпуска.

## Нативное подмножество Task

Текущая ветка разработки имеет ограниченный путь исходного кода `async fn main(): null!`. Он охватывает явно перечисленные примитивы, строки, плоские массивы, одноуровневые Results, запуск/перемещение/ожидание Task, некоторые конструкции управления потоком и ограниченные фоновые операции с файлами/каталогами. Это не полный асинхронный бэкенд опубликованного пакета v0.0.18.

Универсальное сетевое ожидание Task, полное выполнение M:N, произвольные сочетания динамической кучи, все сочетания захватов/функций async и полные длительные испытания остаются незавершёнными. Точное принимаемое и отклоняемое подмножество приведено в [контракте конкурентности](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md).

## Ограничения БД и сети

API БД используют `module.client(config)?`, затем методы получателя; внутри клиента находится ограниченный пул. Проверка с реальным сервером выполняется отдельно для каждого драйвера, системы и коммита. Имитация ABI или успешная компоновка не заменяют реальный запрос.

Redis использует незашифрованный TCP. В MySQL нет единообразной проверки сертификатов с обязательным отказом при ошибке во всех Oracle/MariaDB. Поэтому прямые подключения должны использовать loopback, контролируемые частные сети или проверенный TLS-туннель. Удалённый PostgreSQL требует явного `sslmode=verify-full` и доверенных корневых сертификатов. Нативный TLS в `std.net` не означает, что HTTP, Redis или интерпретатор используют тот же путь.

## Самохостинг и совместимость

Самостоятельно реализованные на Ku лексер и парсер имеют ограниченные подмножества и дифференциальные проверки. Полные собственные парсер, проверяющий модуль, IR и бэкенд пока не завершены; инструменты начальной загрузки всё ещё предоставляет Rust. Версия 0.0.x не обещает совместимость исходников или сгенерированного C/FFI ABI.

Рабочие журналы и прежние результаты CI подтверждают только указанный в них объём и коммит. [Примечания к выпускам](/ru/releases/) отделяют опубликованные изменения от разработки. Этот сайт не утверждает, что все текущие проверки CI ветки, производственные нагрузки или приёмка БД пройдены.

> Дополнения для v0.0.18 ниже пока приведены на английском; остальной перевод сохранён. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

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

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.
