---
title: '플랫폼 및 백엔드 지원'
description: 'Ku 릴리스 플랫폼, 인터프리터, 러너, 네이티브 C, Task 부분집합, LLVM 지원의 차이.'
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

이 문서는 실험적 사전 릴리스 v0.0.18의 확정된 범위를 설명합니다. 실제 공개 상태와 다운로드는 GitHub Releases에서 확인하세요.

## 공개된 CLI 플랫폼

v0.0.18 실험적 사전 릴리스는 Windows x64(MSVC), Linux x64(GNU), macOS arm64(Apple Silicon)를 지원합니다. 플랫폼마다 압축 파일이 다릅니다. 세 시스템을 모두 지원하는 단일 실행 파일은 없습니다. 다른 플랫폼 자산은 검증하지 않았습니다. [설치](/ko/docs/install/)를 참고하세요.

## 실행 경로

| 기능                                      | 인터프리터 `ku run`         | 기본 `ku build` 러너              | 동기 네이티브 C                   | 개발 네이티브 Task                        | LLVM 텍스트 프로토타입 |
| ----------------------------------------- | --------------------------- | --------------------------------- | --------------------------------- | ----------------------------------------- | ---------------------- |
| 기본 언어, 함수, 제어 흐름                | 구현됨                      | 인터프리터 경로                   | 부분집합 구현                     | 제한된 타입 부분집합                      | 제한된 프로토타입      |
| JSON                                      | 지원                        | 지원                              | 지원                              | 미지원                                    | 미지원                 |
| 소스와 독립적인 전체 로컬 가져오기 그래프 | 소스 로드                   | 미지원. 진입 소스만 포함          | 생성 시 확장                      | 지원 부분집합 내 확장                     | 배포 보장 없음         |
| async / await                             | 리소스 제한과 함께 구현     | 인터프리터 경로                   | 별도 부분집합 또는 거부           | 부분 지원. 완전한 async가 아님            | 완전한 지원 없음       |
| PostgreSQL / Redis / MySQL 클라이언트     | 데이터베이스 접근 없음      | 인터프리터 데이터베이스 접근 없음 | 네이티브 전용                     | 일반 Task 대기가 아님                     | 미지원                 |
| TCP / TLS 클라이언트                      | 대응하는 네이티브 경로 없음 | 대응하는 네이티브 경로 없음       | 생성 C 구현. TLS에는 대응 팩 필요 | 완전한 범용 네트워크 대기를 주장하지 않음 | 미지원                 |

기본 빌드는 대응하는 Rust 도구 모음과 링커가 필요합니다. 네이티브 C에는 대상 C 도구 모음이 필요합니다. `ku check` 통과가 어느 빌드의 성공도 증명하지 않습니다. LLVM 명령이 있다고 완전한 릴리스 백엔드를 뜻하지는 않습니다.

## 네이티브 Task 부분집합

현재 개발 브랜치에는 제한된 `async fn main(): null!` 소스 경로가 있습니다. 명시된 기본 타입, 문자열, 평탄 배열, 단일 계층 Result, Task 시작/이동/await, 일부 제어 흐름, 제한된 파일/디렉터리 백그라운드 작업을 포함합니다. 공개된 v0.0.18 패키지의 완전한 async 백엔드가 아닙니다.

범용 Task 네트워크 대기, 완전한 M:N 실행, 임의의 동적 힙 조합, 모든 비동기 캡처/함수 조합, 완전한 장시간 연속 부하 검증은 아직 미완료입니다. 정확한 허용 및 거부 부분집합은 [동시성 계약](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md)을 참고하세요.

## 데이터베이스 및 네트워크 제한

데이터베이스 API는 `module.client(config)?` 다음 수신자 메서드를 사용하며 클라이언트 내부에 제한된 풀이 있습니다. 실제 서버 검증은 드라이버, 시스템, 커밋마다 별도입니다. 가짜 ABI나 링크 성공이 실제 쿼리를 대신하지 않습니다.

Redis는 평문 TCP를 사용합니다. MySQL은 Oracle/MariaDB에서 일관된 fail-closed 인증서 검증을 제공하지 않습니다. 따라서 직접 연결은 루프백, 통제된 사설망, 검증된 TLS 터널을 사용해야 합니다. 원격 PostgreSQL은 명시적인 `sslmode=verify-full`과 신뢰할 수 있는 루트 인증서가 필요합니다. `std.net`의 네이티브 TLS가 HTTP, Redis, 인터프리터에서 같은 경로를 재사용한다는 뜻은 아닙니다.

## 셀프 호스팅 및 호환성

Ku의 셀프 호스팅 렉서와 파서는 한정된 부분집합과 차등 검사를 갖습니다. 완전한 셀프 호스팅 파서, 검사기, IR, 백엔드는 아직 미완료이며 Rust가 계속 부트스트랩 도구 모음을 제공합니다. 0.0.x 버전은 소스나 생성 C/FFI ABI 호환성을 약속하지 않습니다.

작업 로그와 과거 CI 결과가 입증하는 것은 명시된 범위와 커밋뿐입니다. [릴리스 노트](/ko/releases/)로 공개된 변경과 개발 작업을 구분하세요. 이 웹사이트는 현재 브랜치의 모든 CI, 프로덕션 워크로드, 데이터베이스 검증을 통과했다고 보고하지 않습니다.

> v0.0.18 추가 정보는 현재 영어로 제공하며 기존 번역은 유지합니다. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

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
