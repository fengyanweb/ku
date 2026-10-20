---
title: '릴리스 노트'
description: 'Ku v0.0.18 experimental prerelease scope, downloads and known limitations.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
  ]
---

이 문서는 실험적 사전 릴리스 v0.0.18의 확정된 범위를 설명합니다. 실제 공개 상태와 다운로드는 GitHub Releases에서 확인하세요.

> v0.0.18 추가 정보는 현재 영어로 제공하며 기존 번역은 유지합니다. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction. Do not describe the issue as repaired or long-term stability as accepted.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions. General value copy/release follow-up and AST analysis remain paused. No unrelated memory-limit validation is being repeated.

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

## v0.0.17 · 실험적 사전 릴리스

출시 시각은 **2026-10-18(America/Los_Angeles)**이며 UTC로는 `2026-10-19T03:51:10Z`입니다. 릴리스는 `prerelease=true`, `draft=false`입니다. 현재 안정 릴리스는 확인되지 않았습니다.

주요 변경:

- 동기 읽기 전용 대여 매개변수는 `&name: T`를 사용하며 호출 시 일반 인자를 전달합니다. 일반 소유 매개변수는 이동 의미를 유지합니다.
- 셀프 호스팅 파서 부분집합에 중괄호 `while`을 추가했습니다. 중첩과 문장 수는 제한됩니다.
- 크로스 플랫폼 테스트의 표준 출력 연결 해제 검사를 수정했습니다. 업무 API를 추가하지 않습니다.
- 압축 파일에 기본 러너에 맞는 Rust 의존성과 대상 TLS 팩이 포함됩니다. 적절한 외부 도구 모음은 여전히 필요합니다.

다운로드는 Windows x64, Linux x64, macOS arm64를 지원합니다. 릴리스 자산 7개는 플랫폼 압축 파일 3개, 매니페스트 3개, `SHA256SUMS`입니다. [설치](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)와 [전체 릴리스 노트](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)를 참고하세요.

### 호환성 및 제한

Ku는 실험 단계이며 프로덕션용으로 준비되지 않았습니다. 0.0.x 버전은 소스나 생성 C/FFI ABI 호환성을 약속하지 않습니다. 업데이트할 때 관련 네이티브 아티팩트를 다시 컴파일하세요. 비동기 매개변수 대여, 완전한 셀프 호스팅, 완전한 네이티브 async는 이번 릴리스에 포함되지 않습니다.

기본 러너는 진입 소스만 포함하며 가져온 모듈은 원래 위치가 필요합니다. 데이터베이스 클라이언트는 네이티브 전용입니다. 드라이버마다 실제 서버, TLS, 세션 상태 제한을 확인하세요. Ku 자체 코드에는 여전히 오픈 소스 라이선스가 부여되지 않았습니다.
