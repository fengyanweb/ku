---
title: '프로젝트 및 기여'
description: 'Ku 프로젝트 상태, 공개 소스, 라이선스 범위, 재현 가능한 문제 보고.'
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

Ku는 프로그래밍 언어와 도구 체인입니다. 인터프리터, 기본 실행 가능한 runner, 지원 부분집합의 네이티브 C 컴파일을 제공합니다.

## 소스 및 라이선스

Ku와 VS Code 확장은 **Apache-2.0** 라이선스로 배포됩니다. [소스 코드](https://github.com/fengyanweb/ku), [0.0.19 릴리스](https://github.com/fengyanweb/ku/releases/tag/v0.0.19), [라이선스 전문](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE)을 확인하세요.

제삼자 구성 요소에는 각자의 라이선스가 적용됩니다. 릴리스 패키지에는 해당 라이선스 전문과 고지가 포함됩니다.

## 문제 보고

재현 가능한 보고는 저장소의 [Issues](https://github.com/fengyanweb/ku/issues)를 이용하세요. 다음을 포함합니다:

이메일로 의견 보내기: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Ku 버전, 개발 소스의 Git 커밋, 대상 시스템 / 아키텍처.
- 전체 명령, 백엔드, 최소 `.ku` 소스.
- 완전한 진단 또는 구조화된 Error, 예상 출력과 실제 출력.
- 빌드 문제라면 Rust / C 도구 모음 버전 및 로컬 가져오기 관련 여부.

토큰, 비밀번호, 실제 연결 문자열, 민감한 업무 데이터가 포함된 로그는 제출하지 마세요. 성능 보고에는 환경, 부하, 측정 방법, 원시 결과를 포함해야 합니다. 한 번 성공한 표본은 프로덕션 처리량을 입증하지 않습니다.

## 기여하기 전에

변경 범위를 좁게 유지하고 최소 재현 예제, 영향 범위, 검증 결과를 포함하세요. 코드, 문서, 예제는 같은 버전을 기준으로 작성해야 합니다. 기여 내용은 유지 관리자의 검토 후 병합됩니다.

이 사이트는 Ku 코드의 온라인 실행을 제공하지 않습니다.

[시작하기](/ko/docs/getting-started/), [지원 범위](/ko/docs/support/), [릴리스 노트](/ko/releases/)로 이어가세요.
