---
title: '설치 및 다운로드'
description: 'Ku 0.0.18 실험적 사전 릴리스 다운로드와 플랫폼 및 빌드 요구 사항.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.18.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
    'docs/v0.0.18-r1.md',
  ]
slug: ko/v0.0.18/docs/install
---

## 플랫폼 선택

[Ku 0.0.18 다운로드](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| 시스템  | 아키텍처              | 압축 파일                                    |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

위 세 플랫폼의 미리 빌드한 패키지를 제공합니다. 교차 컴파일 지원이 모든 시스템의 배포 패키지를 의미하지는 않습니다.

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

복사 가능한 설치 절차입니다. macOS는 curl·tar·shasum, Linux는 curl·tar·awk·sha256sum, Windows는 PowerShell과 tar.exe가 필요합니다. SHA256 검증 후 Hello Ku를 실행합니다. PATH는 POSIX 자식 셸에서만 바꾸거나 PowerShell에서 복원하므로 전역 설정을 변경하지 않습니다. Windows 소스는 BOM 없는 UTF-8이며 공백 경로를 지원합니다.

### macOS · Apple Silicon

```sh
(
  set -eu
  base="https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1"
  archive="ku-v0.0.18-aarch64-apple-darwin.tar.gz"
  work="$(mktemp -d "${TMPDIR:-/tmp}/Ku install.XXXXXX")"
  cd "$work"
  curl --fail --location --output "$archive" "$base/$archive"
  curl --fail --location --output SHA256SUMS "$base/SHA256SUMS"
  awk -v name="$archive" '$2 == name { print }' SHA256SUMS > archive.sha256
  test "$(wc -l < archive.sha256 | tr -d ' ')" = 1
  shasum -a 256 -c archive.sha256
  mkdir "Ku 0.0.18"
  tar -xzf "$archive" -C "Ku 0.0.18"
  cd "Ku 0.0.18"
  export PATH="$PWD:$PATH"
  ku --version
  cat > hello.ku <<'KU'
fn main() {
    println("Hello Ku")
}
KU
  ku check hello.ku
  ku run hello.ku
  printf 'Ku directory: %s\n' "$PWD"
)
```

### Linux · x64 GNU

```sh
(
  set -eu
  base="https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1"
  archive="ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz"
  work="$(mktemp -d "${TMPDIR:-/tmp}/Ku install.XXXXXX")"
  cd "$work"
  curl --fail --location --output "$archive" "$base/$archive"
  curl --fail --location --output SHA256SUMS "$base/SHA256SUMS"
  awk -v name="$archive" '$2 == name { print }' SHA256SUMS > archive.sha256
  test "$(wc -l < archive.sha256 | tr -d ' ')" = 1
  sha256sum -c archive.sha256
  mkdir "Ku 0.0.18"
  tar -xzf "$archive" -C "Ku 0.0.18"
  cd "Ku 0.0.18"
  export PATH="$PWD:$PATH"
  ku --version
  cat > hello.ku <<'KU'
fn main() {
    println("Hello Ku")
}
KU
  ku check hello.ku
  ku run hello.ku
  printf 'Ku directory: %s\n' "$PWD"
)
```

### Windows · x64

```powershell
$ErrorActionPreference = 'Stop'
$base = 'https://github.com/fengyanweb/ku/releases/download/v0.0.18-r1'
$archive = 'ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz'
$work = Join-Path ([IO.Path]::GetTempPath()) ('Ku install ' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
$archivePath = Join-Path $work $archive
$sumsPath = Join-Path $work 'SHA256SUMS'
Invoke-WebRequest -Uri "$base/$archive" -OutFile $archivePath
Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sumsPath
$pattern = '^[0-9a-fA-F]{64}\s+' + [regex]::Escape($archive) + '$'
$lines = @(Get-Content -LiteralPath $sumsPath | Where-Object { $_ -match $pattern })
if ($lines.Count -ne 1) { throw 'Missing or ambiguous SHA256SUMS entry' }
$expected = ($lines[0] -split '\s+')[0]
$actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
if ($actual -ne $expected) { throw 'SHA256 mismatch; do not extract or run' }
$kuDir = Join-Path $work 'Ku 0.0.18'
New-Item -ItemType Directory -Path $kuDir | Out-Null
tar -xzf $archivePath -C $kuDir
if ($LASTEXITCODE -ne 0) { throw 'Archive extraction failed' }
$hello = @'
fn main() {
    println("Hello Ku")
}
'@
[IO.File]::WriteAllText((Join-Path $kuDir 'hello.ku'), $hello + "`n", [Text.UTF8Encoding]::new($false))
$previousPath = $env:Path
Push-Location -LiteralPath $kuDir
try {
    $env:Path = "$kuDir;$env:Path"
    ku.exe --version
    if ($LASTEXITCODE -ne 0) { throw 'Version check failed' }
    ku.exe check hello.ku
    if ($LASTEXITCODE -ne 0) { throw 'Source check failed' }
    ku.exe run hello.ku
    if ($LASTEXITCODE -ne 0) { throw 'Program failed' }
} finally {
    $env:Path = $previousPath
    Pop-Location
}
Write-Output "Ku directory: $kuDir"
```

```text
ku 0.0.18
Hello Ku
```

## 실행 및 빌드 요구 사항

| 작업                                    | 추가 요구 사항                                                                |
| --------------------------------------- | ----------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | 해당 플랫폼의 Ku CLI                                                          |
| 기본 `ku build hello.ku`                | 대상 플랫폼용 Rust **1.89.0**과 시스템 링커. 포함된 Rust 의존성을 보존해야 함 |
| `ku build --backend c hello.ku`         | 동작하는 대상 플랫폼용 C 컴파일러와 링커                                      |
| 네이티브 데이터베이스 프로그램          | 대응하는 외부 데이터베이스 클라이언트 동적 라이브러리. 패키지에 포함되지 않음 |

기본 러너에는 진입 소스만 포함됩니다. 가져온 모듈은 여전히 원래 위치에서 읽습니다. 전체 로컬 가져오기 그래프를 소스에 의존하지 않고 배포하려면 네이티브 C 컴파일이 필요하며 [백엔드 지원 범위](/ko/v0.0.18/docs/support/)의 제한도 적용됩니다.

기본 runner는 rustc를 호출합니다. RELEASE.json과 동봉 rlib에 맞춰 Rust 1.89.0을 명시적으로 선택하세요. 다른 기본 Rust는 E0514를 일으킬 수 있습니다. Ku는 임시 PATH에 있어야 합니다.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## 해당 소스 빌드

다음 명령으로 배포된 소스 코드를 빌드합니다.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## 제거

현재 아카이브로 설치합니다. 설치 폴더에 만든 소스와 설정을 보관한 뒤 폴더, 직접 추가한 `PATH` 항목과 VS Code 확장 기능을 제거하세요.

다음 단계: [첫 프로그램 실행](/ko/v0.0.18/docs/getting-started/). 설치 계약의 출처: [릴리스 압축 파일](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

`ku supervise`의 플랫폼 조건과 메모리 지표는 [지원 범위](/ko/v0.0.18/docs/support/)에 있습니다. macOS CLI는 프로그램을 실행하지만 이 격리 모드를 네이티브 지원하지 않습니다.
