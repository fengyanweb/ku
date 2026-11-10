---
title: 'نصب و دریافت'
description: 'دریافت پیش‌انتشار آزمایشی Ku 0.0.18 و آشنایی با نیازمندی‌های پلتفرم و ساخت.'
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
slug: fa/v0.0.18/docs/install
---

## انتخاب پلتفرم

[دریافت Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| سیستم   | معماری                | بایگانی                                      |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

بستهٔ آماده برای سه سامانهٔ بالا ارائه می‌شود. کامپایل متقاطع به معنی وجود بستهٔ منتشرشده برای هر سامانه نیست.

## استخراج و بررسی

1. بایگانی را با `SHA256SUMS` انتشار تطبیق دهید. فایل جداگانهٔ `.manifest.json` یکپارچگی بایگانی و فایل‌ها را توصیف می‌کند. چک‌سام‌ها برای بررسی یکپارچگی‌اند و امضای ناشر نیستند.
2. **تمام بایگانی** را در پوشه‌ای خالی استخراج کرده و `README-INSTALL.md` را بخوانید. چیدمان نسبی `ku`، ‏`libku.rlib`، ‏`deps/` و `native-tls/` را حفظ کنید.
3. نسخه را از همان پوشه بررسی کنید. در macOS / Linux:

```sh
./ku --version
```

در Windows PowerShell:

```powershell
.\ku.exe --version
```

نسخهٔ گزارش‌شده باید `0.0.18` باشد. در صورت نیاز پوشه را به `PATH` اضافه کنید و سپس `ku --version` را اجرا کنید. برای یکپارچگی با VS Code، فایل همراه `ku-language-0.0.18.vsix` را نصب کنید.

نصب قابل کپی: macOS به curl و tar و shasum، لینوکس به curl و tar و awk و sha256sum و ویندوز به PowerShell و tar.exe نیاز دارد. بلوک‌ها SHA256 را می‌سنجند و Hello Ku اجرا می‌کنند. PATH فقط در زیرپوستهٔ POSIX تغییر می‌کند یا در PowerShell برمی‌گردد؛ سراسری نیست. سورس ویندوز UTF-8 بدون BOM است و مسیر دارای فاصله پشتیبانی می‌شود.

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

## نیازمندی‌های اجرا و ساخت

| عملیات                                  | نیازمندی‌های اضافی                                                                     |
| --------------------------------------- | -------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI متناسب با پلتفرم                                                                |
| دستور پیش‌فرض `ku build hello.ku`       | Rust **1.89.0** برای مقصد و پیونددهندهٔ سیستم؛ وابستگی‌های Rust همراه بسته را حفظ کنید |
| `ku build --backend c hello.ku`         | کامپایلر C و پیونددهندهٔ سالم برای مقصد                                                |
| برنامهٔ بومی پایگاه داده                | کتابخانه‌های پویای خارجیِ سازگارِ کارخواه پایگاه داده؛ در بسته نیستند                  |

اجراکنندهٔ پیش‌فرض فقط کد ورودی را در خود جای می‌دهد. ماژول‌های واردشده همچنان از محل اصلی بارگذاری می‌شوند. استقرار کل گراف واردسازی محلی بدون وابستگی به کد منبع، به کامپایل C بومی نیاز دارد و همچنان تابع [محدودیت‌های پشتیبانی از مسیرهای اجرا](/fa/v0.0.18/docs/support/) است.

runner پیش‌فرض rustc را اجرا می‌کند. Rust 1.89.0 را مطابق RELEASE.json و rlibهای بسته صریح انتخاب کنید؛ Rust پیش‌فرض دیگر ممکن است E0514 بدهد. Ku باید در PATH موقت باشد.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## ساخت سورس همان نسخه

دستورهای زیر کد منبع منتشرشده را می‌سازند.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## حذف نصب

نصب از آرشیو انجام می‌شود. ابتدا سورس و تنظیمات خود را در پوشهٔ نصب حفظ کنید؛ سپس پوشه، ورودی‌های افزوده‌شدهٔ `PATH` و افزونهٔ VS Code را حذف کنید.

گام بعدی: [اجرای اولین برنامه](/fa/v0.0.18/docs/getting-started/). منبع قرارداد نصب: [بایگانی‌های انتشار](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

شرایط سامانه و معیارهای حافظهٔ `ku supervise` در [پشتیبانی](/fa/v0.0.18/docs/support/) آمده است. CLI در macOS برنامه اجرا می‌کند، اما این جداسازی را بومی پشتیبانی نمی‌کند.
