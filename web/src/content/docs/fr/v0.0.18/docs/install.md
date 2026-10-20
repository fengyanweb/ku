---
title: Installation et téléchargements
description: Téléchargez la préversion expérimentale Ku 0.0.18 et comprenez les
  prérequis de plateforme et de compilation.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - Cargo.toml
  - docs/v0.0.18.md
  - docs/history.md
  - .github/scripts/export-release.py
slug: fr/v0.0.18/docs/install
---

> Les compléments v0.0.18 ci-dessous sont actuellement en anglais ; le reste de la traduction est conservé. [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## Choisir votre plateforme

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| Système | Architecture          | Archive                                      |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Aucune archive pour une autre plateforme n'a été vérifiée. La prise en charge de la compilation croisée ne signifie pas que chaque système ou architecture dispose d'un paquet publié.

## Extraire et vérifier

1. Vérifiez l'archive à l'aide du `SHA256SUMS` de la version. Un `.manifest.json` distinct décrit l'intégrité de l'archive et des fichiers. Les sommes de contrôle vérifient l'intégrité ; ce ne sont pas des signatures de l'éditeur.
2. Extrayez **l'archive entière** dans un répertoire vide et lisez `README-INSTALL.md`. Conservez l'agencement relatif de `ku`, `libku.rlib`, `deps/` et `native-tls/`.
3. Vérifiez la version depuis ce répertoire. Sous macOS / Linux :

```sh
./ku --version
```

Sous Windows PowerShell :

```powershell
.\ku.exe --version
```

La version indiquée devrait être `0.0.18`. Vous pouvez ajouter ce répertoire à `PATH`, puis lancer `ku --version`. Installez le fichier fourni `ku-language-0.0.18.vsix` pour l'intégration VS Code.

## Prérequis d'exécution et de compilation

| Opération                               | Prérequis supplémentaires                                                                                   |
| --------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | CLI Ku pour la plateforme correspondante                                                                    |
| `ku build hello.ku` par défaut          | Rust **1.89.0** pour la cible et son éditeur de liens système ; conserver les dépendances Rust fournies     |
| `ku build --backend c hello.ku`         | Compilateur et éditeur de liens C fonctionnels pour la cible                                                |
| Programme natif de base de données      | Bibliothèques dynamiques externes de client de base de données correspondantes ; elles ne sont pas fournies |

Le lanceur par défaut n'embarque que le source du point d'entrée. Les modules importés sont toujours chargés depuis leur emplacement d'origine. Le déploiement indépendant des sources d'un graphe complet d'importations locales exige une compilation native en C et reste soumis aux [limites de prise en charge des backends](/fr/v0.0.18/docs/support/).

## Essayer le code de développement

Depuis une copie locale du dépôt :

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Désinstaller

Pour une installation par archive, supprimez le répertoire extrait, annulez toute entrée `PATH` ajoutée et désinstallez l'extension VS Code. Préservez d'abord les fichiers source et la configuration que vous y avez créés. Aucun installateur distinct en un clic, aucune installation par gestionnaire de paquets ni commande `ku uninstall` n'a été vérifié.

Étape suivante : [exécuter votre premier programme](/fr/v0.0.18/docs/getting-started/). Source du contrat d'installation : [archives de versions](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/fr/v0.0.18/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
