---
title: 'Instalare și descărcare'
description: 'Descarcă versiunea preliminară experimentală Ku 0.0.18 și înțelege cerințele de platformă și compilare.'
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

> Actualizările v0.0.18 de mai jos sunt momentan în engleză; restul traducerii este păstrat. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## Alege platforma

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| Sistem  | Arhitectură           | Arhivă                                       |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Nu au fost verificate arhive pentru alte platforme. Suportul pentru compilare încrucișată nu înseamnă că fiecare sistem sau arhitectură are un pachet publicat.

## Extragere și verificare

1. Verifică arhiva folosind `SHA256SUMS` al versiunii. Un `.manifest.json` separat descrie integritatea arhivei și fișierelor. Sumele de control verifică integritatea, nu sunt semnături ale editorului.
2. Extrage **întreaga arhivă** într-un director gol și citește `README-INSTALL.md`. Păstrează dispunerea relativă a `ku`, `libku.rlib`, `deps/` și `native-tls/`.
3. Verifică versiunea din acel director. Pe macOS / Linux:

```sh
./ku --version
```

În Windows PowerShell:

```powershell
.\ku.exe --version
```

Versiunea raportată trebuie să fie `0.0.18`. Opțional, adaugă directorul în `PATH`, apoi rulează `ku --version`. Instalează `ku-language-0.0.18.vsix` inclus pentru integrarea VS Code.

## Cerințe de rulare și compilare

| Operație                                | Cerințe suplimentare                                                                     |
| --------------------------------------- | ---------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI pentru platforma corespunzătoare                                                  |
| `ku build hello.ku` implicit            | Rust **1.89.0** pentru țintă și linkerul sistemului; păstrează dependențele Rust incluse |
| `ku build --backend c hello.ku`         | Un compilator C și un linker funcționale pentru țintă                                    |
| Program nativ cu bază de date           | Biblioteci dinamice externe potrivite ale clientului bazei de date; nu sunt incluse      |

Lansatorul implicit încorporează doar sursa punctului de intrare. Modulele importate se încarcă în continuare din locațiile originale. Distribuirea întregului graf de importuri locale independent de surse necesită compilare nativă prin C și rămâne supusă [limitelor backendurilor](/ro/docs/support/).

## Încearcă sursa de dezvoltare

Dintr-o copie locală a depozitului:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Dezinstalare

Pentru instalarea din arhivă, șterge directorul extras, anulează intrarea adăugată în `PATH` și dezinstalează extensia VS Code. Păstrează mai întâi sursele și configurația create acolo. Nu a fost verificat un instalator separat cu un clic, instalarea prin manager de pachete sau comanda `ku uninstall`.

Urmează: [rulează primul program](/ro/docs/getting-started/). Sursa contractului de instalare: [arhivele versiunilor](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/ro/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
