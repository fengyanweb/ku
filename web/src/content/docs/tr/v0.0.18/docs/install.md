---
title: 'Kurulum ve indirme'
description: 'Ku 0.0.18 deneysel ön sürümünü indirin; platform ve derleme gereksinimlerini öğrenin.'
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
slug: tr/v0.0.18/docs/install
---

## Platformunuzu seçin

[Ku 0.0.18 indirin](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| Sistem  | Mimari                | Arşiv                                        |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Belirtilen üç platform için hazır paket sağlanır. Çapraz derleme her sistem için yayınlanmış paket anlamına gelmez.

## Çıkarın ve doğrulayın

1. Arşivi sürümdeki `SHA256SUMS` ile doğrulayın. Ayrı bir `.manifest.json`, arşiv ve dosya bütünlüğünü açıklar. Sağlama toplamları bütünlük denetimidir; yayıncı imzası değildir.
2. **Arşivin tamamını** boş bir dizine çıkarın ve `README-INSTALL.md` dosyasını okuyun. `ku`, `libku.rlib`, `deps/` ve `native-tls/` arasındaki göreli dizilimi koruyun.
3. O dizinden sürümü denetleyin. macOS / Linux üzerinde:

```sh
./ku --version
```

Windows PowerShell içinde:

```powershell
.\ku.exe --version
```

Bildirilen sürüm `0.0.18` olmalıdır. İsterseniz bu dizini `PATH` içine ekleyip `ku --version` çalıştırın. VS Code bütünleşmesi için paketteki `ku-language-0.0.18.vsix` dosyasını kurun.

Kopyalanabilir kurulum: macOS curl, tar, shasum; Linux curl, tar, awk, sha256sum; Windows PowerShell ve tar.exe ister. Bloklar SHA256 doğrular ve Hello Ku çalıştırır. PATH yalnızca POSIX alt kabuğunda değişir veya PowerShell içinde geri yüklenir; küresel ayar değişmez. Windows kaynağı BOM olmadan UTF-8 yazılır ve boşluklu yollar desteklenir.

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

## Çalıştırma ve derleme gereksinimleri

| İşlem                                   | Ek gereksinimler                                                                            |
| --------------------------------------- | ------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Eşleşen platform için Ku CLI                                                                |
| Varsayılan `ku build hello.ku`          | Hedef için Rust **1.89.0** ve sistem bağlayıcısı; paketlenmiş Rust bağımlılıklarını koruyun |
| `ku build --backend c hello.ku`         | Çalışan bir hedef C derleyicisi ve bağlayıcısı                                              |
| Yerel veritabanı programı               | Eşleşen harici veritabanı istemcisi dinamik kütüphaneleri; pakete dahil değildir            |

Varsayılan çalıştırıcı yalnızca giriş kaynağını gömer. İçe aktarılan modüller özgün konumlarından yüklenmeye devam eder. Tam bir yerel içe aktarma grafiğinin kaynaklardan bağımsız dağıtımı, C üzerinden yerel derleme gerektirir ve [arka uç sınırlarına](/tr/v0.0.18/docs/support/) tabidir.

Varsayılan runner rustc çağırır. RELEASE.json ve paket rlibs ile eşleşen Rust 1.89.0 sürümünü açıkça seçin; başka varsayılan Rust E0514 üretebilir. Ku geçici PATH içinde olmalıdır.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Eşleşen kaynağı derleme

Aşağıdaki komutlar yayımlanan kaynak kodu derler.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Kaldırma

Kurulum arşivle yapılır. Kurulum dizinindeki kendi kaynak ve ayarlarınızı saklayın; ardından dizini, eklediğiniz `PATH` girdilerini ve VS Code uzantısını kaldırın.

Sıradaki adım: [ilk programınızı çalıştırın](/tr/v0.0.18/docs/getting-started/). Kurulum sözleşmesinin kaynağı: [sürüm arşivleri](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

`ku supervise` platform koşulları ve bellek ölçüleri [destek](/tr/v0.0.18/docs/support/) sayfasındadır. macOS CLI program çalıştırır, fakat bu yalıtım kipini yerel olarak desteklemez.
