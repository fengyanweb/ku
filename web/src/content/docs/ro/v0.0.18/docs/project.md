---
title: 'Proiect și contribuții'
description: 'Starea proiectului Ku, sursa publică, limitele licențierii și raportarea reproductibilă a problemelor.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
    'docs/v0.0.18-r1.md',
  ]
slug: ro/v0.0.18/docs/project
---

Ku este un limbaj și un set de instrumente: interpretor, runner executabil implicit și compilare C nativă pentru submulțimea suportată.

## Sursă și licențiere

Vezi [sursa GitHub](https://github.com/fengyanweb/ku) și [pagina versiunii](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Sursa poate fi văzută public, dar codul propriu Ku **nu are acordată o licență open source**. Extensia editorului este marcată `UNLICENSED`. Publicarea surselor sau binarelor nu acordă automat o licență open source.

Notificările terților din arhive se aplică doar componentelor respective. Nu stabilesc permisiuni de utilizare, modificare sau redistribuire pentru Ku în sine. Orice autorizare necesară trebuie să vină explicit de la responsabilul proiectului; site-ul nu creează noi angajamente de licențiere.

## Raportează o problemă

Folosește [Issues](https://github.com/fengyanweb/ku/issues) ale depozitului pentru rapoarte reproductibile. Include:

- Versiunea Ku, commitul Git pentru surse de dezvoltare și sistemul / arhitectura țintă.
- Comanda completă, backendul și sursa minimă `.ku`.
- Diagnosticul complet sau Error structurat, ieșirea așteptată și cea efectivă.
- Versiunile instrumentelor Rust / C pentru probleme de compilare și dacă sunt implicate importuri locale.

Nu trimite tokenuri, parole, șiruri reale de conexiune sau jurnale cu date de afaceri sensibile. Rapoartele de performanță trebuie să includă mediul, sarcina, metoda de măsurare și rezultatele brute. Un singur exemplu reușit nu dovedește debitul în producție.

## Înainte de a contribui

Pentru contribuții, oferă reproducere minimă, aria modificărilor și testele relevante. Codul și documentația trebuie să corespundă aceleiași versiuni. Acceptarea contribuțiilor și drepturile de utilizare, modificare și redistribuire Ku se stabilesc cu responsabilul proiectului.

Site-ul nu oferă execuție online a codului Ku.

Continuă cu [primii pași](/ro/v0.0.18/docs/getting-started/), [limitele suportului](/ro/v0.0.18/docs/support/) și [notele versiunilor](/ro/v0.0.18/releases/).
