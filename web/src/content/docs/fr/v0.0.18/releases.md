---
title: 'Notes de version'
description: 'Ku 0.0.18 — Notes de version'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
slug: fr/v0.0.18/releases
---

## v0.0.18

Corrige la réévaluation des récepteurs dans les appels chaînés, les constantes flottantes C natives et `time.date(Time): Date!`, et met à jour l’extension VS Code.

- JSON: `encode`/`decode` partagent `stringify`/`parse`, avec la correction d’aller-retour de `1e20`.

[Télécharger Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Plateformes et backends pris en charge](/fr/v0.0.18/docs/support/)

## v0.0.17 · Préversion expérimentale

Principaux changements :

- Les paramètres empruntés synchrones en lecture seule utilisent `&name: T`, avec des arguments d'appel ordinaires. Les paramètres possédés normaux conservent la sémantique de déplacement.
- Le sous-ensemble du parser auto-hébergé ajoute `while` avec accolades, avec une profondeur d'imbrication et un nombre d'instructions bornés.
- Les archives incluent les dépendances Rust correspondantes pour le lanceur par défaut et un paquet TLS cible. Des chaînes d'outils externes adaptées restent nécessaires.

Les téléchargements couvrent Windows x64, Linux x64 et macOS arm64. Les sept fichiers de la version sont trois archives de plateforme, trois manifestes et `SHA256SUMS`. Voir l'[installation](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) et les [notes de version complètes](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Compatibilité et limites

Ku reste expérimental et n'est pas prêt pour la production. La version 0.0.x ne promet pas de compatibilité des sources ou de l'ABI C/FFI générée. Recompilez les artefacts natifs concernés lors des mises à jour. Les paramètres async empruntés, l'auto-hébergement complet et l'async natif complet sont hors du périmètre de cette version.

Le lanceur par défaut n'embarque que le source d'entrée ; les modules importés exigent toujours leurs emplacements d'origine. Les clients de bases sont natifs uniquement. Vérifiez les limites de serveur réel, TLS et état de session de chaque pilote. Le code propre à Ku ne bénéficie toujours d'aucune concession de licence open source.
