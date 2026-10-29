---
title: 'Proiect și contribuții'
description: 'Starea proiectului Ku, sursa publică, limitele licențierii și raportarea reproductibilă a problemelor.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-28'
sources:
  [
    'README.md',
    'Cargo.toml',
    'LICENSE',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Ku este un limbaj și un set de instrumente: interpretor, runner executabil implicit și compilare C nativă pentru submulțimea suportată.

## Sursă și licențiere

Ku și extensia VS Code sunt distribuite sub licența **Apache-2.0**. Vezi [codul sursă](https://github.com/fengyanweb/ku), [versiunea 0.0.19](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) și [textul licenței](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE).

Componentele terțe își păstrează propriile licențe. Pachetele includ textele licențelor și notificările corespunzătoare.

## Raportează o problemă

Folosește [Issues](https://github.com/fengyanweb/ku/issues) ale depozitului pentru rapoarte reproductibile. Include:

Feedback prin e-mail: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Versiunea Ku, commitul Git pentru surse de dezvoltare și sistemul / arhitectura țintă.
- Comanda completă, backendul și sursa minimă `.ku`.
- Diagnosticul complet sau Error structurat, ieșirea așteptată și cea efectivă.
- Versiunile instrumentelor Rust / C pentru probleme de compilare și dacă sunt implicate importuri locale.

Nu trimite tokenuri, parole, șiruri reale de conexiune sau jurnale cu date de afaceri sensibile. Rapoartele de performanță trebuie să includă mediul, sarcina, metoda de măsurare și rezultatele brute. Un singur exemplu reușit nu dovedește debitul în producție.

## Înainte de a contribui

Păstrează modificările concentrate și include o reproducere minimă, domeniul afectat și rezultatele verificărilor. Codul, documentația și exemplele trebuie să vizeze aceeași versiune. Responsabilii verifică contribuțiile înainte de integrare.

Site-ul nu oferă execuție online a codului Ku.

Continuă cu [primii pași](/ro/docs/getting-started/), [limitele suportului](/ro/docs/support/) și [notele versiunilor](/ro/releases/).
