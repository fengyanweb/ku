---
title: Plateformes et backends pris en charge
description: Distinguez les plateformes publiées de Ku, l'interpréteur, le
  lanceur, C natif, le sous-ensemble Task et LLVM.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/syntax.md
  - docs/v0.0.18.md
  - docs/v0.0.18-worklog.md
  - docs/protocol-foundation.md
  - .github/workflows/native-three-os.yml
slug: fr/v0.0.18/docs/support
---

Cette documentation décrit le périmètre figé de la préversion expérimentale v0.0.18. La publication effective et les téléchargements sont consignés dans GitHub Releases.

## Plateformes CLI publiées

La préversion expérimentale v0.0.18 couvre Windows x64 (MSVC), Linux x64 (GNU) et macOS arm64 (Apple Silicon). Chaque plateforme possède son archive. Il n'existe pas d'exécutable unique pour les trois systèmes. Aucun autre fichier de plateforme n'a été vérifié ; voir l'[installation](/fr/v0.0.18/docs/install/).

## Chemins d'exécution

| Capacité                                                    | Interpréteur `ku run`                 | Lanceur par défaut `ku build`                            | C natif synchrone                                     | Task natif en développement                            | Prototype texte LLVM            |
| ----------------------------------------------------------- | ------------------------------------- | -------------------------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------ | ------------------------------- |
| Bases du langage, fonctions, contrôle                       | Implémentées                          | Chemin interpréteur                                      | Sous-ensemble implémenté                              | Sous-ensemble typé restreint                           | Prototype limité                |
| JSON                                                        | Pris en charge                        | Pris en charge                                           | Pris en charge                                        | Non pris en charge                                     | Non pris en charge              |
| Graphe local complet d'importations indépendant des sources | Charge les sources                    | Non pris en charge ; seul le point d'entrée est embarqué | Développé à la génération                             | Développé dans le sous-ensemble pris en charge         | Aucune promesse de déploiement  |
| async / await                                               | Implémenté avec limites de ressources | Chemin interpréteur                                      | Sous-ensemble dédié ou refus                          | Partiel, pas d'async complet                           | Pas de prise en charge complète |
| Clients PostgreSQL / Redis / MySQL                          | Aucun accès aux bases                 | Aucun accès interprété aux bases                         | Natifs uniquement                                     | Pas d'attente Task générique                           | Non pris en charge              |
| Client TCP / TLS                                            | Aucun chemin natif correspondant      | Aucun chemin natif correspondant                         | Implémentation C générée ; TLS exige un paquet adapté | Aucune affirmation d'attente réseau générique complète | Non pris en charge              |

Les compilations par défaut exigent la chaîne Rust correspondante et l'éditeur de liens. C natif exige une chaîne C pour la cible. La réussite de `ku check` ne prouve le fonctionnement d'aucune des deux compilations. Une commande LLVM n'implique pas un backend de publication complet.

## Le sous-ensemble natif Task

La branche de développement actuelle possède un chemin source restreint pour `async fn main(): null!`. Il couvre les primitives explicitement énumérées, chaînes, tableaux plats, Results à une couche, démarrage/déplacement/await de Task, une partie du contrôle et des opérations bornées sur les fichiers/répertoires en arrière-plan. Ce n'est pas un backend async complet dans le paquet publié v0.0.18.

L'attente réseau Task générique, l'exécution M:N complète, les combinaisons arbitraires de tas dynamique, toutes les combinaisons captures/fonctions async et les tests d'endurance longue durée complets restent inachevés. Voir les sous-ensembles précisément acceptés et refusés dans le [contrat de concurrence](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md).

## Limites des bases de données et du réseau

Les API de bases utilisent `module.client(config)?`, puis des méthodes du récepteur, avec un pool borné dans le client. La validation sur serveur réel est distincte pour chaque pilote, système et commit. Une ABI simulée ou une édition de liens réussie ne remplace pas une véritable requête.

Redis utilise TCP en clair. MySQL ne dispose pas d'une vérification uniforme des certificats avec refus en cas d'erreur entre Oracle/MariaDB. Les connexions directes doivent donc utiliser l'interface de bouclage, des réseaux privés contrôlés ou un tunnel TLS vérifié. PostgreSQL distant exige explicitement `sslmode=verify-full` et des certificats racines de confiance. Le TLS natif de `std.net` ne signifie pas que HTTP, Redis ou l'interpréteur réutilisent ce chemin.

## Auto-hébergement et compatibilité

Le lexer et le parser auto-hébergés de Ku disposent de sous-ensembles bornés et de vérifications différentielles. Un parser, vérificateur, IR et backend entièrement auto-hébergés restent inachevés ; Rust fournit toujours la chaîne d'amorçage. La version 0.0.x ne promet pas de compatibilité des sources ou de l'ABI C/FFI générée.

Les journaux de travail et les résultats CI antérieurs établissent uniquement leur portée et leur commit déclarés. Utilisez les [notes de version](/fr/v0.0.18/releases/) pour distinguer les changements publiés du développement. Ce site n'annonce pas la réussite de toute la CI de la branche actuelle, des charges de production ou des validations de bases de données.

> Les compléments v0.0.18 ci-dessous sont actuellement en anglais ; le reste de la traduction est conservé. [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction. Do not describe the issue as repaired or long-term stability as accepted.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions. General value copy/release follow-up and AST analysis remain paused. No unrelated memory-limit validation is being repeated.

### Process memory boundary

| Host | `ku supervise` guarantee and prerequisite |
| --- | --- |
| Linux | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment. |
| macOS | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge. |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.
