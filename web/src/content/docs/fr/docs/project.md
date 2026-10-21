---
title: 'Projet et contributions'
description: 'État du projet Ku, sources publiques, limites de licence et signalements reproductibles.'
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

Cette documentation décrit le périmètre figé de la préversion expérimentale v0.0.18. La publication effective et les téléchargements sont consignés dans GitHub Releases. Le projet se nomme **Ku programming language / Ku 编程语言**, avec une CLI appelée `ku`. Ku est un petit langage de programmation et interpréteur en développement. Sa version publique actuelle est la préversion expérimentale v0.0.18.

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

Le [contrat syntaxique](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) et le [contrat sémantique](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) définissent le comportement du langage. Examinez ensemble CI, implémentation et contrats. Un objectif du journal de travail n'est pas une fonctionnalité implémentée.

Gardez les changements ciblés et expliquez la reproduction, la cause, la portée et la validation. Examinez le contenu du site avec les sources correspondantes. Préservez les tests existants, les changements de l'arbre de travail et les preuves uniques. Le mainteneur décide de l'acceptation des contributions et des accords de licence ; ce site n'invente aucun accord de contribution.

Ce site est statique. Ses textes, sa navigation et son code ne nécessitent aucun backend métier. La recherche charge un index Pagefind dans le navigateur ; le site ne propose pas d'exécution distante de code Ku.

Poursuivez avec les [premiers pas](/fr/docs/getting-started/), les [limites de prise en charge](/fr/docs/support/) et les [notes de version](/fr/releases/).
