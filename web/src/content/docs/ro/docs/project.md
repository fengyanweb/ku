---
title: 'Proiect și contribuții'
description: 'Starea proiectului Ku, sursa publică, limitele licențierii și raportarea reproductibilă a problemelor.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Documentația curentă vizează Ku 0.0.19, o prelansare experimentală cu aceeași versiune pentru CLI și extensia VS Code. Disponibilitatea, fișierele și sumele de control sunt stabilite de GitHub Release efectiv.

**Ku programming language / Ku 编程语言** · CLI: `ku`.

## Sursă și licențiere

[GitHub](https://github.com/fengyanweb/ku) · [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)

Ku și extensia VS Code sunt oferite sub Apache License 2.0. Consultați [LICENSE](https://github.com/fengyanweb/ku/blob/main/LICENSE) și [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE). Componentele terțe își păstrează drepturile de autor și licențele proprii.

## Raportează o problemă

Folosește [Issues](https://github.com/fengyanweb/ku/issues) ale depozitului pentru rapoarte reproductibile. Include:

Feedback prin e-mail: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- Versiunea Ku, commitul Git pentru surse de dezvoltare și sistemul / arhitectura țintă.
- Comanda completă, backendul și sursa minimă `.ku`.
- Diagnosticul complet sau Error structurat, ieșirea așteptată și cea efectivă.
- Versiunile instrumentelor Rust / C pentru probleme de compilare și dacă sunt implicate importuri locale.

Nu trimite tokenuri, parole, șiruri reale de conexiune sau jurnale cu date de afaceri sensibile. Rapoartele de performanță trebuie să includă mediul, sarcina, metoda de măsurare și rezultatele brute. Un singur exemplu reușit nu dovedește debitul în producție.

## Înainte de a contribui

[Contractul de sintaxă](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) și [contractul semantic](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) definesc comportamentul limbajului. Verifică împreună CI, implementarea și contractele. O țintă din jurnalul de lucru nu este o funcție implementată.

Păstrează schimbările concentrate și explică reproducerea, cauza, aria și verificarea. Revizuiește conținutul site-ului alături de sursa relevantă. Păstrează testele existente, modificările arborelui de lucru și dovezile unice. Responsabilul proiectului decide acceptarea contribuțiilor și condițiile de licențiere; site-ul nu inventează un acord de contribuție.

Acest site este static. Textul, navigația și codul nu necesită un backend de afaceri. Căutarea încarcă un index Pagefind în browser; site-ul nu oferă execuție Ku la distanță.

Continuă cu [primii pași](/ro/docs/getting-started/), [limitele suportului](/ro/docs/support/) și [notele versiunilor](/ro/releases/).
