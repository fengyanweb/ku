---
title: '플랫폼 및 백엔드 지원'
description: 'Ku 릴리스 플랫폼, 인터프리터, 러너, 네이티브 C, Task 부분집합, LLVM 지원의 차이.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Ku 0.0.19 다운로드

Ku 0.0.19는 실험적 사전 릴리스입니다. macOS ARM64(Apple Silicon), Windows x64, Linux x64(GNU) 패키지가 공개되어 있습니다.

[패키지 및 설치 방법](/ko/docs/install/)

## 실행 방식

- **`ku run`**: 언어 기능, 일반 JSON, 동기 HTTP 핸들러를 제공하는 인터프리터입니다. 데이터베이스 클라이언트는 구현되어 있지 않습니다.
- **`ku build`**: 기본 runner는 진입 파일을 포함하지만 로컬 import에는 원본 소스 파일이 여전히 필요합니다. Rust **1.89.0**과 시스템 링커가 필요합니다.
- **`ku build --backend c`**: 동기 JSON, `http.json`을 포함한 HTTP, 네이티브 PostgreSQL·Redis·MySQL 클라이언트를 지원합니다. 대상 C 컴파일러, 링커, 호환 클라이언트 라이브러리가 필요합니다. 데이터베이스 작업은 자동 재시도하지 않습니다.
- **Task**: 네이티브 Task는 타입이 정해진 하위 집합에서 시작/`await`, 타이머, 일부 I/O, 이름 있는 async HTTP 핸들러와 특정 PG 쿼리를 지원합니다. 이 경로의 JSON은 스칼라 값 객체 리터럴 인코딩만 지원하며 일반 디코딩은 제공하지 않습니다. 완전한 M:N과 범용 I/O 지원은 아직 완료되지 않았습니다.
- **LLVM**: LLVM 텍스트 백엔드는 여전히 제한적인 프로토타입입니다.

## 알려진 문제

세 플랫폼의 릴리스 검사와 패키지 검증은 통과했습니다. 모든 프로덕션 사용을 검증했다는 뜻은 아닙니다.

- **HTTP 504:** 과거에 발생했지만 최신 짧은 재검사에서는 재현되지 않았습니다. 원인은 아직 확인되지 않았습니다.
- **취소 중 지연 급증:** 최신 30초 재검사에서 짧은 요청 60,000건이 모두 정확했습니다. p99는 **1.557 ms**, 최댓값은 **98.809 ms**였으며 20 ms를 넘는 요청이 남아 있습니다.

공식 30분 안정성 검증은 아직 통과하지 못했습니다. 짧은 측정으로 이를 대신할 수는 없습니다.

전체 참고 문서: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

문제 제보: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
