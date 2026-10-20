---
title: '설치 및 다운로드'
description: 'Ku 0.0.18 실험적 사전 릴리스 다운로드와 플랫폼 및 빌드 요구 사항.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

> v0.0.18 추가 정보는 현재 영어로 제공하며 기존 번역은 유지합니다. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## 플랫폼 선택

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| 시스템  | 아키텍처              | 압축 파일                                    |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

다른 플랫폼용 압축 파일은 검증하지 않았습니다. 크로스 컴파일 지원은 모든 운영체제나 아키텍처용 패키지가 공개되었다는 뜻이 아닙니다.

## 압축 해제 및 검증

1. 릴리스의 `SHA256SUMS`와 대조하여 압축 파일을 검증합니다. 별도의 `.manifest.json`에는 압축 파일과 개별 파일의 무결성 정보가 있습니다. 체크섬은 무결성 확인 수단이며 배포자 서명이 아닙니다.
2. 빈 디렉터리에 **압축 파일 전체**를 풀고 `README-INSTALL.md`를 읽으세요. `ku`, `libku.rlib`, `deps/`, `native-tls/`의 상대적 배치를 유지해야 합니다.
3. 해당 디렉터리에서 버전을 확인합니다. macOS / Linux에서는:

```sh
./ku --version
```

Windows PowerShell에서는:

```powershell
.\ku.exe --version
```

표시되는 버전은 `0.0.18`이어야 합니다. 필요하면 이 디렉터리를 `PATH`에 추가한 뒤 `ku --version`을 실행하세요. VS Code 연동에는 포함된 `ku-language-0.0.18.vsix`를 설치합니다.

## 실행 및 빌드 요구 사항

| 작업                                    | 추가 요구 사항                                                                |
| --------------------------------------- | ----------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | 해당 플랫폼의 Ku CLI                                                          |
| 기본 `ku build hello.ku`                | 대상 플랫폼용 Rust **1.89.0**과 시스템 링커. 포함된 Rust 의존성을 보존해야 함 |
| `ku build --backend c hello.ku`         | 동작하는 대상 플랫폼용 C 컴파일러와 링커                                      |
| 네이티브 데이터베이스 프로그램          | 대응하는 외부 데이터베이스 클라이언트 동적 라이브러리. 패키지에 포함되지 않음 |

기본 러너에는 진입 소스만 포함됩니다. 가져온 모듈은 여전히 원래 위치에서 읽습니다. 전체 로컬 가져오기 그래프를 소스에 의존하지 않고 배포하려면 네이티브 C 컴파일이 필요하며 [백엔드 지원 범위](/ko/docs/support/)의 제한도 적용됩니다.

## 개발 소스 사용해 보기

로컬에 체크아웃한 저장소에서 실행합니다:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## 제거

압축 파일로 설치한 경우 압축을 푼 디렉터리를 제거하고 추가한 `PATH` 항목을 되돌린 뒤 VS Code 확장을 제거하세요. 그곳에 만든 소스 파일과 설정은 먼저 보존해야 합니다. 별도의 원클릭 설치 프로그램, 패키지 관리자를 통한 설치, `ku uninstall` 명령은 검증하지 않았습니다.

다음 단계: [첫 프로그램 실행](/ko/docs/getting-started/). 설치 계약의 출처: [릴리스 압축 파일](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/ko/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
