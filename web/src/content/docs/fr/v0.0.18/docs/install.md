---
title: 'Installation et téléchargements'
description: 'Téléchargez la préversion expérimentale Ku 0.0.18 et comprenez les prérequis de plateforme et de compilation.'
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
slug: fr/v0.0.18/docs/install
---

## Choisir votre plateforme

[Télécharger Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · `SHA256SUMS` · `.manifest.json`

| Système | Architecture          | Archive                                      |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Des paquets précompilés sont fournis pour ces trois plateformes. La compilation croisée ne signifie pas qu’un paquet est publié pour chaque système.

## Extraire et vérifier

1. Vérifiez l'archive à l'aide du `SHA256SUMS` de la version. Un `.manifest.json` distinct décrit l'intégrité de l'archive et des fichiers. Les sommes de contrôle vérifient l'intégrité ; ce ne sont pas des signatures de l'éditeur.
2. Extrayez **l'archive entière** dans un répertoire vide et lisez `README-INSTALL.md`. Conservez l'agencement relatif de `ku`, `libku.rlib`, `deps/` et `native-tls/`.
3. Vérifiez la version depuis ce répertoire. Sous macOS / Linux :

```sh
./ku --version
```

Sous Windows PowerShell :

```powershell
.\ku.exe --version
```

La version indiquée devrait être `0.0.18`. Vous pouvez ajouter ce répertoire à `PATH`, puis lancer `ku --version`. Installez le fichier fourni `ku-language-0.0.18.vsix` pour l'intégration VS Code.

Installation à copier : macOS nécessite curl, tar et shasum ; Linux curl, tar, awk et sha256sum ; Windows PowerShell et tar.exe. Les blocs vérifient SHA256 et exécutent Hello Ku. Le PATH ne change que dans le sous-shell POSIX ou est restauré dans PowerShell, jamais globalement. La source Windows est en UTF-8 sans BOM ; les chemins avec espaces sont gérés.

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

## Prérequis d'exécution et de compilation

| Opération                               | Prérequis supplémentaires                                                                                   |
| --------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | CLI Ku pour la plateforme correspondante                                                                    |
| `ku build hello.ku` par défaut          | Rust **1.89.0** pour la cible et son éditeur de liens système ; conserver les dépendances Rust fournies     |
| `ku build --backend c hello.ku`         | Compilateur et éditeur de liens C fonctionnels pour la cible                                                |
| Programme natif de base de données      | Bibliothèques dynamiques externes de client de base de données correspondantes ; elles ne sont pas fournies |

Le lanceur par défaut n'embarque que le source du point d'entrée. Les modules importés sont toujours chargés depuis leur emplacement d'origine. Le déploiement indépendant des sources d'un graphe complet d'importations locales exige une compilation native en C et reste soumis aux [limites de prise en charge des backends](/fr/v0.0.18/docs/support/).

Le runner par défaut invoque rustc. Sélectionnez explicitement Rust 1.89.0 selon RELEASE.json et les rlibs fournis ; un autre Rust par défaut peut provoquer E0514. Ku doit être dans le PATH temporaire.

```sh
rustup run 1.89.0 ku build hello.ku
```

PowerShell:

```powershell
rustup run 1.89.0 ku.exe build hello.ku
```

## Construire les sources correspondantes

Les commandes suivantes compilent le code source publié.

```sh
git clone --branch v0.0.18-r1 --depth 1 https://github.com/fengyanweb/ku.git
cd ku
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

## Désinstaller

L’installation utilise des archives. Sauvegardez vos sources et réglages dans le dossier d’installation, puis supprimez ce dossier, vos ajouts au `PATH` et l’extension VS Code.

Étape suivante : [exécuter votre premier programme](/fr/v0.0.18/docs/getting-started/). Source du contrat d'installation : [archives de versions](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/history.md).

Les prérequis et mesures mémoire de `ku supervise` figurent dans la [prise en charge](/fr/v0.0.18/docs/support/). La CLI macOS exécute les programmes, mais ce mode d’isolation n’y est pas disponible nativement.
