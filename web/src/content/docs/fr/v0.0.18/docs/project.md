---
title: 'Projet et contributions'
description: 'État du projet Ku, sources publiques, limites de licence et signalements reproductibles.'
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
slug: fr/v0.0.18/docs/project
---

Ku est un langage de programmation et une chaîne d’outils : interpréteur, runner exécutable par défaut et compilation C native d’un sous-ensemble pris en charge.

## Sources et licence

Consultez les [sources sur GitHub](https://github.com/fengyanweb/ku) et la [page de version](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Les sources sont consultables publiquement, mais le code propre à Ku **ne bénéficie d'aucune concession de licence open source**. L'extension d'éditeur est marquée `UNLICENSED`. Publier des sources ou des binaires n'accorde pas automatiquement une licence open source.

Les mentions de tiers présentes dans les archives s'appliquent uniquement à leurs composants respectifs. Elles n'accordent pas de droits d'utilisation, de modification ou de redistribution de Ku lui-même. Toute autorisation nécessaire doit venir explicitement du mainteneur ; ce site ne crée aucun nouvel engagement de licence.

## Signaler un problème

Utilisez les [Issues](https://github.com/fengyanweb/ku/issues) du dépôt pour les signalements reproductibles. Incluez :

- La version de Ku, le commit Git pour les sources de développement et le système / l'architecture cible.
- La commande complète, le backend et un code source `.ku` minimal.
- Le diagnostic complet ou l'Error structurée, la sortie attendue et la sortie réelle.
- Les versions des chaînes Rust / C pour les problèmes de compilation, et la présence éventuelle d'importations locales.

N'envoyez pas de jetons, mots de passe, véritables chaînes de connexion ni journaux contenant des données métier sensibles. Les rapports de performances doivent préciser l'environnement, la charge, la méthode de mesure et les résultats bruts. Un seul exemple réussi n'établit pas un débit en production.

## Avant de contribuer

Pour contribuer, fournissez reproduction minimale, périmètre et tests pertinents. Code et documentation doivent viser le même état. Consultez le responsable pour l’acceptation des contributions et les droits d’utilisation, modification et redistribution de Ku.

Le site ne propose pas d’exécution de code Ku en ligne.

Poursuivez avec les [premiers pas](/fr/v0.0.18/docs/getting-started/), les [limites de prise en charge](/fr/v0.0.18/docs/support/) et les [notes de version](/fr/v0.0.18/releases/).
