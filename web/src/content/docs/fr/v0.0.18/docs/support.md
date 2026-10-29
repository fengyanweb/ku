---
title: 'Plateformes et backends pris en charge'
description: "Distinguez les plateformes publiées de Ku, l'interpréteur, le lanceur, C natif, le sous-ensemble Task et LLVM."
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
    'docs/v0.0.18-r1.md',
  ]
slug: fr/v0.0.18/docs/support
---

## Plateformes CLI publiées

La préversion expérimentale v0.0.18 couvre Windows x64 (MSVC), Linux x64 (GNU) et macOS arm64 (Apple Silicon). Chaque plateforme possède son archive. Il n'existe pas d'exécutable unique pour les trois systèmes. Aucun autre fichier de plateforme n'a été vérifié ; voir l'[installation](/fr/v0.0.18/docs/install/).

## Chemins d'exécution

| Capacité                                                    | Interpréteur `ku run`                 | Lanceur par défaut `ku build`                            | C natif synchrone                                     | Tasks natives (sous-ensemble)                          | Prototype texte LLVM            |
| ----------------------------------------------------------- | ------------------------------------- | -------------------------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------ | ------------------------------- |
| Bases du langage, fonctions, contrôle                       | Implémentées                          | Chemin interpréteur                                      | Sous-ensemble implémenté                              | Sous-ensemble typé restreint                           | Prototype limité                |
| JSON                                                        | Pris en charge                        | Pris en charge                                           | Pris en charge                                        | Non pris en charge                                     | Non pris en charge              |
| Graphe local complet d'importations indépendant des sources | Charge les sources                    | Non pris en charge ; seul le point d'entrée est embarqué | Développé à la génération                             | Développé dans le sous-ensemble pris en charge         | Aucune promesse de déploiement  |
| async / await                                               | Implémenté avec limites de ressources | Chemin interpréteur                                      | Sous-ensemble dédié ou refus                          | Partiel, pas d'async complet                           | Pas de prise en charge complète |
| Clients PostgreSQL / Redis / MySQL                          | Aucun accès aux bases                 | Aucun accès interprété aux bases                         | Natifs uniquement                                     | Pas d'attente Task générique                           | Non pris en charge              |
| Client TCP / TLS                                            | Aucun chemin natif correspondant      | Aucun chemin natif correspondant                         | Implémentation C générée ; TLS exige un paquet adapté | Aucune affirmation d'attente réseau générique complète | Non pris en charge              |

Les compilations par défaut exigent la chaîne Rust correspondante et l'éditeur de liens. C natif exige une chaîne C pour la cible. La réussite de `ku check` ne prouve le fonctionnement d'aucune des deux compilations. Une commande LLVM n'implique pas un backend de publication complet.

## Le sous-ensemble natif Task

La version inclut le chemin C natif restreint `async fn main(): null!` : types primitifs listés, chaînes, tableaux plats, Results à un niveau, démarrage/déplacement/await de Task, contrôle de flux sélectionné et opérations de fichiers/répertoires bornées en arrière-plan. Les opérations TCP/TLS locales utilisent un pool bloquant fixe. Ce n’est ni une readiness réseau générale ni un ordonnanceur M:N complet ; consultez la référence de concurrence de cette version.

L'attente réseau Task générique, l'exécution M:N complète, les combinaisons arbitraires de tas dynamique, toutes les combinaisons captures/fonctions async et les tests d'endurance longue durée complets restent inachevés. Voir les sous-ensembles précisément acceptés et refusés dans le [contrat de concurrence](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).

## Limites des bases de données et du réseau

Les API de bases utilisent `module.client(config)?`, puis des méthodes du récepteur, avec un pool borné dans le client. La validation sur serveur réel est distincte pour chaque pilote, système et commit. Une ABI simulée ou une édition de liens réussie ne remplace pas une véritable requête.

Redis utilise TCP en clair. MySQL ne dispose pas d'une vérification uniforme des certificats avec refus en cas d'erreur entre Oracle/MariaDB. Les connexions directes doivent donc utiliser l'interface de bouclage, des réseaux privés contrôlés ou un tunnel TLS vérifié. PostgreSQL distant exige explicitement `sslmode=verify-full` et des certificats racines de confiance. Le TLS natif de `std.net` ne signifie pas que HTTP, Redis ou l'interpréteur réutilisent ce chemin.

## Auto-hébergement et compatibilité

Le lexer et le parser auto-hébergés de Ku disposent de sous-ensembles bornés et de vérifications différentielles. Un parser, vérificateur, IR et backend entièrement auto-hébergés restent inachevés ; Rust fournit toujours la chaîne d'amorçage. La version 0.0.x ne promet pas de compatibilité des sources ou de l'ABI C/FFI générée.

## Périmètre et limites de v0.0.18

- JSON: `encode`/`decode` partagent `stringify`/`parse`, avec la correction d’aller-retour de `1e20`.
- Les Tasks natives restent le sous-ensemble borné listé ; les corrections d’admission à l’arrêt et de nettoyage n’impliquent pas M:N complet ou suspension réseau générale.
- HTTP offre readiness socket, files, réponses et keep-alive bornés, plus des diagnostics bornés optionnels ; cela ne valide pas toutes les charges.
- `ku supervise` contrôle les workers de programmes entiers, admission, octets des files, sorties et nettoyage ; ni worker par requête ou Task, ni application automatique à `ku run`.

**Problème HTTP connu :** des erreurs HTTP 504 peuvent survenir par intermittence. Leur cause reste indéterminée.

### Limites mémoire des processus

| Limites mémoire des processus | `ku supervise`                                                                                                                                                                                                                                                          |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux                         | Mémoire comptabilisée par cgroup v2 ; racine de domaine déléguée et accessible en écriture avec `memory`, `pids`, `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max`, `cgroup.kill`. L’absence de droits ou contrôles entraîne un refus avant l’exécution. |
| Windows                       | Des Job Objects imbriqués limitent chaque worker et le groupe selon la mémoire virtuelle engagée, pas le RSS. L’engagement initial est vérifié avant de libérer le thread applicatif suspendu ; la politique hôte peut refuser l’affectation au Job.                    |
| macOS                         | Ce mode d’isolation n’est pas pris en charge nativement et est refusé avant le démarrage. Utilisez un hôte ou une VM Linux correctement configurés si nécessaire ; aucun transfert automatique vers une VM.                                                             |

Le domaine du worker compte les allocations Ku et tierces. Comptabilité cgroup Linux et engagement Windows sont des mesures différentes. `memory.max` peut autoriser un dépassement transitoire ; aucun plafond absolu de toute mémoire physique à tout instant n’est garanti.

Le coordinateur n’a pas de limite mémoire OS stricte ; réserves et comptabilité d’admission ne constituent pas un RSS précis. Le tas complet d’une Task n’est pas mesuré exactement ; pools, caches et allocations des bibliothèques ne peuvent lui être attribués précisément. Bases externes, hôte/VM, compilateurs et `ku run` ordinaire ne sont pas automatiquement soumis aux limites des workers.

Si l’OS termine un worker, le coordinateur signale l’échec et ne libère les réserves qu’après nettoyage confirmé. Certaines allocations peuvent échouer préalablement, mais l’épuisement d’une bibliothèque tierce ne devient pas forcément un Result ordinaire. **Les opérations de base de données ne sont jamais rejouées automatiquement** ; la fin d’un worker ne prouve aucun rollback et les effets peuvent rester inconnus.

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
