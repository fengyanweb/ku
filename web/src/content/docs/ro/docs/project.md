---
title: 'Proiect și contribuții'
description: 'Starea proiectului Ku, sursa publică, limitele licențierii și raportarea reproductibilă a problemelor.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

Documentația descrie domeniul fixat al prelansării experimentale v0.0.18. Publicarea efectivă și descărcările sunt înregistrate în GitHub Releases. Proiectul este **Ku programming language / Ku 编程语言**, cu CLI numit `ku`. Ku este un limbaj de programare mic și un interpretor în dezvoltare. Versiunea publică actuală este versiunea preliminară experimentală v0.0.18.

## Sursă și licențiere

Codul propriu Ku din instantaneele sursei care includ fișierul de licență și extensia actuală a editorului sunt distribuite sub [Apache License 2.0](https://github.com/fengyanweb/ku/blob/main/LICENSE) (`Apache-2.0`); vezi [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE) pentru mențiunile de atribuire. Consultă [sursa GitHub](https://github.com/fengyanweb/ku) și [pagina versiunii](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Componentele terților își păstrează propriile licențe.

Arhivele v0.0.18-r1 și ale versiunilor anterioare păstrează materialele originale de licențiere care le însoțesc; această schimbare a licenței sursei nu reîmpachetează acele arhive. Utilizarea compilatorului Ku nu impune publicarea codului sursă al propriilor programe. La distribuirea programelor care conțin cod din mediul de execuție Ku, respectă cerințele de licență și atribuire aplicabile, inclusiv notificările terților.

## Raportează o problemă

Folosește [Issues](https://github.com/fengyanweb/ku/issues) ale depozitului pentru rapoarte reproductibile. Include:

- Versiunea Ku, commitul Git pentru surse de dezvoltare și sistemul / arhitectura țintă.
- Comanda completă, backendul și sursa minimă `.ku`.
- Diagnosticul complet sau Error structurat, ieșirea așteptată și cea efectivă.
- Versiunile instrumentelor Rust / C pentru probleme de compilare și dacă sunt implicate importuri locale.

Nu trimite tokenuri, parole, șiruri reale de conexiune sau jurnale cu date de afaceri sensibile. Rapoartele de performanță trebuie să includă mediul, sarcina, metoda de măsurare și rezultatele brute. Un singur exemplu reușit nu dovedește debitul în producție.

## Înainte de a contribui

[Contractul de sintaxă](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) și [contractul semantic](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) definesc comportamentul limbajului. Verifică împreună CI, implementarea și contractele. O țintă din jurnalul de lucru nu este o funcție implementată.

Păstrează schimbările concentrate și explică reproducerea, cauza, aria și verificarea. Revizuiește conținutul site-ului alături de sursa relevantă. Păstrează testele existente, modificările arborelui de lucru și dovezile unice. Responsabilul proiectului decide acceptarea contribuțiilor și condițiile de licențiere; site-ul nu inventează un acord de contribuție.

Acest site este static. Textul, navigația și codul nu necesită un backend de afaceri. Căutarea încarcă un index Pagefind în browser; site-ul nu oferă execuție Ku la distanță.

Continuă cu [primii pași](/ro/docs/getting-started/), [limitele suportului](/ro/docs/support/) și [notele versiunilor](/ro/releases/).
