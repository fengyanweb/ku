---
title: 'Εγκατάσταση και λήψη'
description: 'Κατεβάστε την πειραματική προέκδοση Ku 0.0.18 και κατανοήστε τις απαιτήσεις πλατφόρμας και κατασκευής.'
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
slug: el/v0.0.18/docs/install
---

## Επιλέξτε πλατφόρμα

[Λήψη Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| Σύστημα | Αρχιτεκτονική         | Αρχείο                                       |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Προμεταγλωττισμένα πακέτα παρέχονται για τις τρεις παραπάνω πλατφόρμες. Η διασταυρούμενη μεταγλώττιση δεν σημαίνει δημοσιευμένο πακέτο για κάθε σύστημα.

## Αποσυμπίεση και επαλήθευση

1. Επαληθεύστε το αρχείο με το `SHA256SUMS` της έκδοσης. Ένα ξεχωριστό `.manifest.json` περιγράφει την ακεραιότητα του συμπιεσμένου αρχείου και των περιεχομένων του. Τα αθροίσματα ελέγχου ελέγχουν ακεραιότητα· δεν είναι υπογραφές του εκδότη.
2. Αποσυμπιέστε **ολόκληρο το αρχείο** σε κενό κατάλογο και διαβάστε το `README-INSTALL.md`. Διατηρήστε τη σχετική διάταξη των `ku`, `libku.rlib`, `deps/` και `native-tls/`.
3. Ελέγξτε την έκδοση από εκείνον τον κατάλογο. Σε macOS / Linux:

```sh
./ku --version
```

Σε Windows PowerShell:

```powershell
.\ku.exe --version
```

Η αναφερόμενη έκδοση πρέπει να είναι `0.0.18`. Προαιρετικά προσθέστε τον κατάλογο στο `PATH` και εκτελέστε `ku --version`. Εγκαταστήστε το παρεχόμενο `ku-language-0.0.18.vsix` για ενσωμάτωση με το VS Code.

Εγκατάσταση με αντιγραφή: macOS απαιτεί curl, tar, shasum, Linux curl, tar, awk, sha256sum και Windows PowerShell με tar.exe. Τα τμήματα ελέγχουν SHA256 και εκτελούν Hello Ku. Το PATH ισχύει μόνο στο POSIX subshell ή επαναφέρεται στο PowerShell· δεν αλλάζει καθολικά. Η πηγή Windows γράφεται UTF-8 χωρίς BOM και υποστηρίζονται διαδρομές με κενά.

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

## Απαιτήσεις εκτέλεσης και κατασκευής

| Ενέργεια                                | Πρόσθετες απαιτήσεις                                                                                    |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI για την αντίστοιχη πλατφόρμα                                                                     |
| Προεπιλεγμένο `ku build hello.ku`       | Rust **1.89.0** για τον στόχο και ο συνδέτης του συστήματος· διατηρήστε τις παρεχόμενες εξαρτήσεις Rust |
| `ku build --backend c hello.ku`         | Λειτουργικός μεταγλωττιστής C και συνδέτης για τον στόχο                                                |
| Εγγενές πρόγραμμα βάσης δεδομένων       | Αντίστοιχες εξωτερικές δυναμικές βιβλιοθήκες πελάτη βάσης δεδομένων· δεν περιλαμβάνονται                |

Ο προεπιλεγμένος εκκινητής ενσωματώνει μόνο την πηγή του σημείου εισόδου. Οι εισαγόμενες μονάδες εξακολουθούν να φορτώνονται από τις αρχικές θέσεις τους. Η ανάπτυξη πλήρους τοπικού γράφου εισαγωγών ανεξάρτητα από τις πηγές απαιτεί εγγενή μεταγλώττιση μέσω C και υπόκειται στα [όρια υποστήριξης backend](/el/v0.0.18/docs/support/).

Το προεπιλεγμένο runner καλεί rustc. Επιλέξτε ρητά Rust 1.89.0 σύμφωνα με RELEASE.json και τα παρεχόμενα rlibs· διαφορετικό προεπιλεγμένο Rust μπορεί να προκαλέσει E0514. Το Ku πρέπει να βρίσκεται στο προσωρινό PATH.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Κατασκευή της αντίστοιχης πηγής

Οι παρακάτω εντολές μεταγλωττίζουν τον δημοσιευμένο πηγαίο κώδικα.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Απεγκατάσταση

Η εγκατάσταση γίνεται από αρχεία. Κρατήστε τις δικές σας πηγές και ρυθμίσεις στον φάκελο εγκατάστασης και αφαιρέστε έπειτα φάκελο, πρόσθετες εγγραφές `PATH` και επέκταση VS Code.

Επόμενο: [εκτελέστε το πρώτο σας πρόγραμμα](/el/v0.0.18/docs/getting-started/). Πηγή της σύμβασης εγκατάστασης: [αρχεία εκδόσεων](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Προϋποθέσεις και μέτρα μνήμης του `ku supervise` βρίσκονται στην [υποστήριξη](/el/v0.0.18/docs/support/). Το CLI macOS εκτελεί προγράμματα, αλλά δεν υποστηρίζει εγγενώς αυτή την απομόνωση.
