---
title: 'Projet et contributions'
description: 'État du projet Ku, sources publiques, limites de licence et signalements reproductibles.'
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

Ku est un langage de programmation et une chaîne d’outils : interpréteur, runner exécutable par défaut et compilation C native d’un sous-ensemble pris en charge.

## Sources et licence

Ku et son extension VS Code sont distribués sous **Apache-2.0**. Consultez le [code source](https://github.com/fengyanweb/ku), la [version 0.0.19](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) et le [texte de la licence](https://github.com/fengyanweb/ku/blob/v0.0.19/LICENSE).

Les composants tiers conservent leurs propres licences. Les paquets publiés contiennent les textes et mentions correspondants.

## Signaler un problème

Utilisez les [Issues](https://github.com/fengyanweb/ku/issues) du dépôt pour les signalements reproductibles. Incluez :

Retour par e-mail: [kagura.gc@qq.com](mailto:kagura.gc@qq.com)

- La version de Ku, le commit Git pour les sources de développement et le système / l'architecture cible.
- La commande complète, le backend et un code source `.ku` minimal.
- Le diagnostic complet ou l'Error structurée, la sortie attendue et la sortie réelle.
- Les versions des chaînes Rust / C pour les problèmes de compilation, et la présence éventuelle d'importations locales.

N'envoyez pas de jetons, mots de passe, véritables chaînes de connexion ni journaux contenant des données métier sensibles. Les rapports de performances doivent préciser l'environnement, la charge, la méthode de mesure et les résultats bruts. Un seul exemple réussi n'établit pas un débit en production.

## Avant de contribuer

Gardez les changements ciblés et fournissez une reproduction minimale, le périmètre et les vérifications pertinentes. Le code, la documentation et les exemples doivent viser la même version. Les responsables examinent les contributions avant leur intégration.

Le site ne propose pas d’exécution de code Ku en ligne.

Poursuivez avec les [premiers pas](/fr/docs/getting-started/), les [limites de prise en charge](/fr/docs/support/) et les [notes de version](/fr/releases/).
