---
title: 'Erreurs et dépannage'
description: "Diagnostiquez les erreurs d'installation, d'importation, de propriété, de JSON et de capacités des backends Ku."
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
  ]
---

Cette documentation décrit le périmètre figé de la préversion expérimentale v0.0.18. La publication effective et les téléchargements sont consignés dans GitHub Releases. Les diagnostics du compilateur peuvent évoluer pendant le développement expérimental. Lors d'un signalement, précisez la version CLI, le commit Git, le backend, la commande complète et un code source minimal.

## Identifier l'exécutable Ku

```sh
ku --version
ku -h
ku check hello.ku
```

Si `ku` est introuvable, lancez l'exécutable depuis son répertoire d'extraction. Utilisez `ku.exe` sous Windows. Si Rust ou des bibliothèques d'édition de liens manquent à la compilation par défaut, consultez les [prérequis d'installation](/fr/docs/install/) et conservez toute la structure de l'archive au lieu de copier uniquement la CLI.

## Lire les diagnostics

La vérification habituelle indique le fichier, l'emplacement dans le source, un extrait et les conseils de correction disponibles. Les intégrations d'outils peuvent utiliser :

```sh
ku check --json hello.ku
```

La sortie utilise **JSON Lines**, avec un diagnostic par ligne. Les champs comprennent `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes` et `helps`. Une réussite est silencieuse ; l'absence de sortie JSON ne signifie pas que la vérification n'a pas été exécutée.

| Diagnostic | Cause fréquente                            | Action                                                                                      |
| ---------- | ------------------------------------------ | ------------------------------------------------------------------------------------------- |
| `E0105`    | Utilisation de `let`                       | Déclarer la variable par sa première affectation                                            |
| `E0302`    | Condition `if` / `while` non booléenne     | Écrire une comparaison booléenne explicite                                                  |
| `E0601`    | Module inconnu de la bibliothèque standard | Vérifier le chemin réel du module, par exemple `std.json`                                   |
| `E0605`    | Importation explicite obligatoire absente  | Importer le module, par exemple `import fs from "std.fs"`                                   |
| `E0901`    | Lecture d'une valeur déplacée              | Cloner explicitement une valeur indépendante ou utiliser l'emprunt synchrone pris en charge |
| `E0913`    | Paramètre async emprunté                   | Utiliser un paramètre possédé ; cloner chez l'appelant si l'original est nécessaire         |
| `E0918`    | Écriture de `&value` au point d'appel      | Passer un argument ordinaire ; la signature déclare l'emprunt                               |

Ce tableau résume les diagnostics enregistrés sans répertorier toutes les erreurs. Le registre actuellement faisant autorité est [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.18/src/error.rs).

## Les erreurs d'exécution suivent un contrat distinct

Les valeurs `domain/code` d'Error à l'exécution sont distinctes des codes `E` du compilateur. Un JSON invalide peut renvoyer `json/parse_error` ; une clé d'objet absente peut renvoyer `object/missing_key`. Utilisez `?` après un appel susceptible d'échouer ou lisez ses champs dans `try/catch`.

```ku
import json from "std.json"

fn main() {
    try {
        json.parse("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

Ce programme pédagogique déclenche volontairement une erreur d'analyse récupérable. La valeur `byte N` signalée est un décalage en octets UTF-8 commençant à zéro. Ce n'est pas un nombre de caractères pour les textes chinois ou les émojis.

## Une compilation native est refusée

La réussite de `ku check` établit uniquement que la vérification du frontend a réussi. C natif et LLVM ont chacun des limites de capacité. Consultez la [matrice de prise en charge](/fr/docs/support/) lorsqu'une construction est explicitement refusée. Augmenter un délai ou réussir avec un autre backend ne valide pas le chemin non pris en charge.

`ku build --native file.ku` sans `-o` écrit seulement du C. Pour compiler et lier un exécutable, utilisez `ku build --backend c file.ku` et installez la chaîne d'outils C de la cible.

Les erreurs de base de données `execution_unknown` et `execution_completed_without_result` ne doivent pas déclencher de répétition automatique. Conservez les éléments de preuve de la requête et de l'opération métier, puis établissez d'abord l'état réel de l'exécution. La vérification du site local ne nécessite aucune connexion à une base de production.

Pour en savoir plus, consultez le [guide des diagnostics du dépôt](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/diagnostics.md).
