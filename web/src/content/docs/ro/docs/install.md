---
title: 'Instalare și descărcare'
description: 'Descarcă Ku 0.0.20 pentru calculatorul tău. Fiecare pachet include instrumentul de linie de comandă, interpretorul și extensia VS Code.'
kuVersion: '0.0.20'
verifiedAt: '2026-11-09'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.20.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Descarcă **Ku 0.0.20** pentru calculatorul tău. Fiecare pachet include instrumentul de linie de comandă, interpretorul și extensia VS Code.

## 1. Descarcă Ku

| Sistem  | Arhitectură           | Descarcă                                                                                                                          |
| ------- | --------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / ARM64 | [Descarcă macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-aarch64-apple-darwin.tar.gz)     |
| Windows | x64                   | [Descarcă Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-pc-windows-msvc.tar.gz) |
| Linux   | x64 / GNU             | [Descarcă Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-unknown-linux-gnu.tar.gz) |

## 2. Extrage și rulează

Extrage **întregul pachet** într-un dosar gol, de exemplu `ku` în dosarul tău de utilizator. Păstrează toate fișierele împreună; nu muta doar executabilul. Deschide un terminal în acel dosar.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

Ar trebui să apară `ku 0.0.20`. Acum poți rula și verifica fișiere `.ku`; aceste operații nu necesită instalarea Rust.

<details>
<summary>Opțional: folosește ku din orice dosar</summary>

Dacă ai extras în `~/ku`, adaugă această linie în fișierul de inițializare al shell-ului, precum `~/.zshrc` sau `~/.bashrc`:

```sh
export PATH="$HOME/ku:$PATH"
```

În Windows, deschide «Editați variabilele de mediu pentru cont», editează `Path` al utilizatorului și adaugă calea completă a dosarului extras, de exemplu `%USERPROFILE%\ku`. Păstrează intrările existente.

Deschide un terminal nou și rulează `ku --version`.

</details>

## 3. Instalează extensia VS Code

În vizualizarea Extensii din VS Code, alege **… → Install from VSIX…**, selectează `ku-language-0.0.20.vsix` din dosarul extras și reîncarcă fereastra.

Dacă Ku nu este în `PATH`, setează `ku.executablePath` la calea completă către `ku` (`ku.exe` în Windows).

<details>
<summary>Verifică descărcarea</summary>

Compară SHA-256 al pachetului cu [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.20/SHA256SUMS). [Pagina versiunii](https://github.com/fengyanweb/ku/releases/tag/v0.0.20) oferă și un `.manifest.json` pentru fiecare pachet, cu sumele de verificare ale fișierelor. Alte instrucțiuni se află în `README-INSTALL.md` din pachet.

</details>

<details>
<summary>Compilarea propriilor programe Ku</summary>

`ku run` și `ku check` nu necesită Rust. `ku build` implicit necesită un linker de sistem și identitatea exactă a compilatorului Rust înregistrată în SDK-ul instalat; pachetele v0.0.20 folosesc **Rust 1.99.1**. `ku build --backend c` necesită un compilator C și un linker pentru platforma țintă. Programele native pentru baze de date necesită și bibliotecile client corespunzătoare, care nu sunt incluse.

</details>

<details>
<summary>Dezinstalare</summary>

Păstrează sursele și configurațiile create de tine, apoi șterge dosarul extras, intrarea adăugată în `PATH` și extensia VS Code.

</details>

[În continuare: rulează primul program](/ro/docs/getting-started/)
