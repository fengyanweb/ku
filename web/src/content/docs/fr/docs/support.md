---
title: 'Plateformes et backends pris en charge'
description: "Distinguez les plateformes publiées de Ku, l'interpréteur, le lanceur, C natif, le sous-ensemble Task et LLVM."
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Télécharger Ku 0.0.19

Ku 0.0.19 est une préversion expérimentale. Les paquets disponibles couvrent macOS ARM64 (Apple Silicon), Windows x64 et Linux x64 (GNU).

[Paquets et instructions d’installation](/fr/docs/install/)

## Modes d’exécution

- **`ku run`**: Interpréteur pour le langage, le JSON général et les gestionnaires HTTP synchrones ; pas de clients de bases de données.
- **`ku build`**: Le runner par défaut embarque le fichier d’entrée ; les imports locaux ont toujours besoin de leurs sources. Il nécessite Rust **1.89.0** et un éditeur de liens système.
- **`ku build --backend c`**: Prend en charge le JSON synchrone, HTTP avec `http.json` et les clients natifs PostgreSQL, Redis et MySQL. Nécessite un compilateur C, un éditeur de liens et des bibliothèques clientes adaptés à la cible. Les opérations de base de données ne sont jamais relancées automatiquement.
- **Task**: Le chemin Task natif prend en charge un sous-ensemble typé avec démarrage/`await`, temporisateurs, certaines opérations d’E/S, gestionnaires HTTP async nommés et certaines requêtes PG. Le JSON y permet seulement l’encodage de littéraux objets à valeurs scalaires ; le décodage général n’est pas disponible. Le M:N complet et la couverture générale des E/S restent inachevés.
- **LLVM**: Le backend texte LLVM reste un prototype limité.

## Problèmes connus

Les contrôles de publication et des paquets ont réussi sur les trois plateformes. Cela ne constitue pas une validation générale pour la production.

- **HTTP 504 :** des erreurs ont été observées par le passé, sans reproduction lors du dernier bref contrôle ; leur cause reste indéterminée.
- **Pics de latence pendant l’annulation :** lors du dernier contrôle de 30 secondes, les 60 000 requêtes courtes étaient correctes. Le p99 était de **1.557 ms** et le maximum de **98.809 ms** ; certaines requêtes dépassaient encore 20 ms.

La validation formelle de stabilité sur 30 minutes n’a pas encore réussi. Les mesures courtes ne la remplacent pas.

Référence complète: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

Signaler un problème: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
