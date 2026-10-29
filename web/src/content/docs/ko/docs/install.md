---
title: '설치 및 다운로드'
description: 'Ku 0.0.19 실험적 사전 릴리스 다운로드와 플랫폼 및 빌드 요구 사항.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

컴퓨터에 맞는 **Ku 0.0.19**를 다운로드하세요. 이 실험적 사전 릴리스의 각 패키지에는 명령줄 도구, 인터프리터, VS Code 확장이 포함되어 있습니다.

## 1. Ku 다운로드

| 시스템  | 아키텍처              | 다운로드                                                                                                                          | 크기    |
| ------- | --------------------- | --------------------------------------------------------------------------------------------------------------------------------- | ------- |
| macOS   | Apple Silicon / ARM64 | [다운로드 macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)     | 39.8 MB |
| Windows | x64                   | [다운로드 Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz) | 44.1 MB |
| Linux   | x64 / GNU             | [다운로드 Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) | 48.3 MB |

## 2. 압축 해제 후 실행

**전체 패키지**를 사용자 폴더의 `ku` 같은 빈 폴더에 압축 해제하세요. 모든 파일을 함께 보관하고 실행 파일만 옮기지 마세요. 해당 폴더에서 터미널을 엽니다.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

`ku 0.0.19`가 표시되면 준비가 끝납니다. `.ku` 파일 실행과 검사에는 Rust를 설치할 필요가 없습니다.

<details>
<summary>선택 사항: 모든 폴더에서 ku 사용</summary>

`~/ku`에 압축을 풀었다면 셸 시작 파일(예: `~/.zshrc` 또는 `~/.bashrc`)에 다음 줄을 추가하세요:

```sh
export PATH="$HOME/ku:$PATH"
```

Windows에서 «계정의 환경 변수 편집»을 열고 사용자 `Path`에 압축 해제한 폴더의 전체 경로(예: `%USERPROFILE%\ku`)를 추가하세요. 기존 항목은 유지하세요.

새 터미널을 열고 `ku --version`을 실행하세요.

</details>

## 3. VS Code 확장 설치

VS Code 확장 보기에서 **… → VSIX에서 설치…**를 선택하고 압축 해제한 폴더의 `ku-language-0.0.19.vsix`를 연 다음 창을 다시 로드하세요.

Ku를 `PATH`에 추가하지 않았다면 `ku.executablePath`를 `ku`(Windows에서는 `ku.exe`)의 전체 경로로 설정하세요.

<details>
<summary>다운로드 검증</summary>

패키지의 SHA-256을 [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS)와 비교할 수 있습니다. [릴리스 페이지](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)에는 각 패키지의 파일 체크섬이 담긴 `.manifest.json`도 있습니다. 자세한 내용은 패키지의 `README-INSTALL.md`를 참고하세요.

</details>

<details>
<summary>자신의 Ku 프로그램 컴파일</summary>

`ku run`과 `ku check`는 해당 플랫폼의 CLI만 필요합니다. 기본 `ku build`에는 Rust **1.89.0**과 시스템 링커가 추가로 필요합니다. `ku build --backend c`에는 대상 C 컴파일러와 링커가 필요합니다. 네이티브 데이터베이스 프로그램에는 패키지에 포함되지 않은 해당 데이터베이스 클라이언트 라이브러리도 필요합니다.

</details>

<details>
<summary>제거</summary>

직접 만든 소스와 설정을 보관한 다음 압축 해제한 폴더, 추가한 `PATH` 항목, VS Code 확장을 삭제하세요.

</details>

[다음 단계: 첫 프로그램 실행](/ko/docs/getting-started/)
