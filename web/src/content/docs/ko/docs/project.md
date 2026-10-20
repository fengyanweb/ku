---
title: '프로젝트 및 기여'
description: 'Ku 프로젝트 상태, 공개 소스, 라이선스 범위, 재현 가능한 문제 보고.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

이 문서는 실험적 사전 릴리스 v0.0.18의 확정된 범위를 설명합니다. 실제 공개 상태와 다운로드는 GitHub Releases에서 확인하세요. 프로젝트 이름은 **Ku programming language / Ku 编程语言**이며 CLI 이름은 `ku`입니다. Ku는 개발 중인 작은 프로그래밍 언어와 인터프리터입니다. 현재 공개 릴리스는 v0.0.18 실험적 사전 릴리스입니다.

## 소스 및 라이선스

[GitHub 소스](https://github.com/fengyanweb/ku)와 [릴리스 페이지](https://github.com/fengyanweb/ku/releases/tag/v0.0.18)를 확인하세요. 소스는 공개적으로 볼 수 있지만 Ku 자체 코드에는 **오픈 소스 라이선스가 부여되지 않았습니다**. 편집기 확장은 `UNLICENSED`로 표시됩니다. 소스나 바이너리를 공개해도 자동으로 오픈 소스 라이선스가 부여되지는 않습니다.

릴리스 압축 파일의 제삼자 고지는 해당 구성 요소에만 적용됩니다. Ku 자체의 사용, 수정, 재배포 권한을 부여하지 않습니다. 필요한 허가는 유지 관리자가 명시적으로 제공해야 합니다. 이 웹사이트는 새로운 라이선스 약속을 만들지 않습니다.

## 문제 보고

재현 가능한 보고는 저장소의 [Issues](https://github.com/fengyanweb/ku/issues)를 이용하세요. 다음을 포함합니다:

- Ku 버전, 개발 소스의 Git 커밋, 대상 시스템 / 아키텍처.
- 전체 명령, 백엔드, 최소 `.ku` 소스.
- 완전한 진단 또는 구조화된 Error, 예상 출력과 실제 출력.
- 빌드 문제라면 Rust / C 도구 모음 버전 및 로컬 가져오기 관련 여부.

토큰, 비밀번호, 실제 연결 문자열, 민감한 업무 데이터가 포함된 로그는 제출하지 마세요. 성능 보고에는 환경, 부하, 측정 방법, 원시 결과를 포함해야 합니다. 한 번 성공한 표본은 프로덕션 처리량을 입증하지 않습니다.

## 기여하기 전에

언어 동작은 [문법 계약](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md)과 [의미 계약](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md)이 정의합니다. CI, 구현, 계약을 함께 확인하세요. 작업 로그의 목표는 구현된 기능이 아닙니다.

변경 범위를 좁히고 재현 방법, 원인, 범위, 검증을 설명하세요. 웹사이트 내용을 관련 소스와 함께 검토합니다. 기존 테스트, 작업 트리 변경, 고유한 증거를 보존하세요. 기여 수락과 라이선스 조건은 유지 관리자가 결정하며 이 웹사이트는 기여 계약을 만들어 내지 않습니다.

이 웹사이트는 정적입니다. 본문, 탐색, 코드에는 업무 백엔드가 필요하지 않습니다. 검색은 브라우저에서 Pagefind 인덱스를 불러옵니다. 사이트는 Ku 코드의 원격 실행을 제공하지 않습니다.

[시작하기](/ko/docs/getting-started/), [지원 범위](/ko/docs/support/), [릴리스 노트](/ko/releases/)로 이어가세요.
