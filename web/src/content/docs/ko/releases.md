---
title: '릴리스 노트'
description: 'Ku 0.0.18 — 릴리스 노트'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
---

## 0.0.19

현재 문서는 실험적 사전 릴리스 Ku 0.0.19를 대상으로 하며 CLI와 VS Code 확장 버전도 동일합니다. 공개 상태, 파일 및 체크섬은 실제 GitHub Release에서 확인하세요.

[Release · SHA256SUMS](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [Installation](/ko/docs/install/) · [Release notes (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.en.md) · [Known issues (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.en.md)

## v0.0.18

호출 체인의 수신자 중복 평가, 네이티브 C float 상수와 `time.date(Time): Date!`를 수정하고 VS Code 확장 기능을 갱신합니다.

- JSON: `encode`/`decode`는 `stringify`/`parse`와 같으며 `1e20` 왕복 수정을 포함합니다.

[Ku 0.0.18 다운로드](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [플랫폼 및 백엔드 지원](/ko/v0.0.18/docs/support/)

## v0.0.17 · 실험적 사전 릴리스

주요 변경:

- 동기 읽기 전용 대여 매개변수는 `&name: T`를 사용하며 호출 시 일반 인자를 전달합니다. 일반 소유 매개변수는 이동 의미를 유지합니다.
- 셀프 호스팅 파서 부분집합에 중괄호 `while`을 추가했습니다. 중첩과 문장 수는 제한됩니다.
- 크로스 플랫폼 테스트의 표준 출력 연결 해제 검사를 수정했습니다. 업무 API를 추가하지 않습니다.
- 압축 파일에 기본 러너에 맞는 Rust 의존성과 대상 TLS 팩이 포함됩니다. 적절한 외부 도구 모음은 여전히 필요합니다.

다운로드는 Windows x64, Linux x64, macOS arm64를 지원합니다. 릴리스 자산 7개는 플랫폼 압축 파일 3개, 매니페스트 3개, `SHA256SUMS`입니다. [설치](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)와 [전체 릴리스 노트](https://github.com/fengyanweb/ku/releases/tag/v0.0.17)를 참고하세요.

### 호환성 및 제한

Ku는 실험 단계이며 프로덕션용으로 준비되지 않았습니다. 0.0.x 버전은 소스나 생성 C/FFI ABI 호환성을 약속하지 않습니다. 업데이트할 때 관련 네이티브 아티팩트를 다시 컴파일하세요. 비동기 매개변수 대여, 완전한 셀프 호스팅, 완전한 네이티브 async는 이번 릴리스에 포함되지 않습니다.

기본 러너는 진입 소스만 포함하며 가져온 모듈은 원래 위치가 필요합니다. 데이터베이스 클라이언트는 네이티브 전용입니다. 드라이버마다 실제 서버, TLS, 세션 상태 제한을 확인하세요. Ku 자체 코드에는 여전히 오픈 소스 라이선스가 부여되지 않았습니다.
