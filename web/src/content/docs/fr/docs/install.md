---
title: 'Installation et téléchargements'
description: 'Téléchargez Ku 0.0.20 pour votre ordinateur. Chaque paquet contient l’outil en ligne de commande, l’interpréteur et l’extension VS Code.'
kuVersion: '0.0.20'
verifiedAt: '2026-11-09'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.20.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Téléchargez **Ku 0.0.20** pour votre ordinateur. Chaque paquet contient l’outil en ligne de commande, l’interpréteur et l’extension VS Code.

## 1. Télécharger Ku

| Système | Architecture          | Télécharger                                                                                                                          |
| ------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| macOS   | Apple Silicon / ARM64 | [Télécharger macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-aarch64-apple-darwin.tar.gz)     |
| Windows | x64                   | [Télécharger Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-pc-windows-msvc.tar.gz) |
| Linux   | x64 / GNU             | [Télécharger Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-unknown-linux-gnu.tar.gz) |

## 2. Extraire et lancer

Extrayez le **paquet complet** dans un dossier vide, par exemple `ku` dans votre dossier utilisateur. Conservez tous les fichiers ensemble ; ne déplacez pas uniquement l’exécutable. Ouvrez un terminal dans ce dossier.

macOS / Linux :

```sh
./ku --version
```

Windows PowerShell :

```powershell
.\ku.exe --version
```

La commande doit afficher `ku 0.0.20`. Vous pouvez maintenant exécuter et vérifier les fichiers `.ku` ; ces opérations ne nécessitent pas Rust.

<details>
<summary>Facultatif : utiliser ku depuis n’importe quel dossier</summary>

Si vous avez extrait le paquet dans `~/ku`, ajoutez cette ligne au fichier de démarrage de votre shell, par exemple `~/.zshrc` ou `~/.bashrc` :

```sh
export PATH="$HOME/ku:$PATH"
```

Sous Windows, ouvrez « Modifier les variables d’environnement de votre compte », modifiez le `Path` utilisateur et ajoutez le chemin complet du dossier extrait, par exemple `%USERPROFILE%\ku`. Conservez les entrées existantes.

Ouvrez un nouveau terminal et exécutez `ku --version`.

</details>

## 3. Installer l’extension VS Code

Dans la vue Extensions de VS Code, choisissez **… → Installer à partir de VSIX…**, sélectionnez `ku-language-0.0.20.vsix` dans le dossier extrait, puis rechargez la fenêtre.

Si Ku n’est pas dans le `PATH`, définissez `ku.executablePath` sur le chemin complet de `ku` (`ku.exe` sous Windows).

<details>
<summary>Vérifier le téléchargement</summary>

Comparez le SHA-256 du paquet avec [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.20/SHA256SUMS). La [page de la version](https://github.com/fengyanweb/ku/releases/tag/v0.0.20) fournit également un `.manifest.json` par paquet avec les sommes de contrôle des fichiers. Consultez aussi `README-INSTALL.md` dans le paquet.

</details>

<details>
<summary>Compiler vos propres programmes Ku</summary>

`ku run` et `ku check` ne nécessitent pas Rust. `ku build` exige un éditeur de liens système et l’identité exacte du compilateur Rust enregistrée dans le SDK installé ; les paquets v0.0.20 utilisent **Rust 1.99.1**. `ku build --backend c` exige un compilateur C et un éditeur de liens pour la cible. Les programmes natifs de base de données nécessitent aussi les bibliothèques clientes correspondantes, non incluses.

</details>

<details>
<summary>Désinstaller</summary>

Conservez les sources et configurations que vous avez créées, puis supprimez le dossier extrait, l’entrée ajoutée au `PATH` et l’extension VS Code.

</details>

[Suite : exécuter votre premier programme](/fr/docs/getting-started/)
