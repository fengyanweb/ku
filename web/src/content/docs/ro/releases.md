---
title: 'Notele versiunilor'
description: 'Ku 0.0.18 — Notele versiunilor'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
---

## 0.0.19

Documentația curentă vizează Ku 0.0.19, o prelansare experimentală cu aceeași versiune pentru CLI și extensia VS Code. Disponibilitatea, fișierele și sumele de control sunt stabilite de GitHub Release efectiv.

[Release · SHA256SUMS](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [Installation](/ro/docs/install/) · [Release notes (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.en.md) · [Known issues (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.en.md)

## v0.0.18

Corectează evaluarea repetată a receptorilor în lanțuri de apel, constantele float C native și `time.date(Time): Date!`, actualizând extensia VS Code.

- JSON: `encode`/`decode` împart `stringify`/`parse`, cu corecția dus-întors pentru `1e20`.

[Descarcă Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Suport pentru platforme și backenduri](/ro/v0.0.18/docs/support/)

## v0.0.17 · Versiune preliminară experimentală

Schimbări principale:

- Parametrii sincroni împrumutați doar pentru citire folosesc `&name: T`, cu argumente obișnuite la apel. Parametrii obișnuiți deținuți păstrează semantica de mutare.
- Submulțimea parserului autoimplementat adaugă `while` cu acolade, cu limite de imbricare și număr de instrucțiuni.
- Arhivele includ dependențe Rust potrivite pentru lansatorul implicit și un pachet TLS pentru țintă. Instrumentele externe corespunzătoare rămân necesare.

Descărcările acoperă Windows x64, Linux x64 și macOS arm64. Cele șapte fișiere Release sunt trei arhive de platformă, trei manifeste și `SHA256SUMS`. Vezi [instalarea](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) și [notele complete ale versiunii](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Compatibilitate și limite

Ku rămâne experimental și nu este pregătit pentru producție. Versiunea 0.0.x nu promite compatibilitate a sursei sau a ABI C/FFI generat. Recompilează artefactele native relevante la actualizare. Parametrii async împrumutați, autoimplementarea completă și async nativ complet nu fac parte din această versiune.

Lansatorul implicit încorporează doar sursa punctului de intrare; modulele importate încă necesită locațiile originale. Clienții de baze de date sunt doar nativi. Verifică serverul real, TLS și limitele stării de sesiune ale fiecărui driver. Codul propriu Ku încă nu are acordată o licență open source.
