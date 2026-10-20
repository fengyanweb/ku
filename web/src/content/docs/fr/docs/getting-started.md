---
title: 'Premiers pas'
description: 'Écrivez Hello Ku, vérifiez votre code et distinguez interprétation et compilation.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources: ['examples/hello.ku', 'README.md', 'docs/syntax.md', 'docs/v0.0.18.md']
---

Cette documentation décrit le périmètre figé de la préversion expérimentale v0.0.18. La publication effective et les téléchargements sont consignés dans GitHub Releases. Cette page utilise la syntaxe de base et les commandes CLI déjà présentes dans v0.0.17. Commencez par [installer Ku](/fr/docs/install/) et vérifiez que `ku --version` fonctionne.

## Écrire votre premier programme

Créez `hello.ku` avec le même contenu que `examples/hello.ku` dans le dépôt :

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` est le point d'entrée. `println` écrit une ligne de texte. Ku n'utilise ni `let` ni `let mut`.

## Vérifier et exécuter

Depuis le répertoire du fichier :

```sh
ku check hello.ku
ku run hello.ku
```

`check` recherche les erreurs lexicales, syntaxiques et sémantiques élémentaires sans exécuter le programme. La sortie standard attendue à l'exécution est :

```text
Hello Ku
```

Si Ku n'est pas dans `PATH`, lancez l'exécutable avec son chemin d'installation complet. Sous Windows, il se nomme `ku.exe`.

## Variables et fonctions

Ce fragment de syntaxe de base peut être placé dans `main` :

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

La première affectation déclare une variable. Un type explicite s'écrit `name:type = value`. `print` écrit sans retour à la ligne automatique ; `println` en ajoute un.

## Construire un exécutable

La page d'installation indique les outils de compilation nécessaires. Pour le backend par défaut :

```sh
ku build hello.ku
```

Le backend par défaut empaquette un lanceur avec interpréteur. La compilation native en C nécessite un compilateur C distinct :

```sh
ku build --backend c hello.ku
```

La sortie par défaut se trouve dans `.ku/build/<profile>/` ; utilisez le chemin exact indiqué par la CLI. Les deux backends prennent en charge des fonctionnalités différentes. La réussite de `ku check` ne prouve pas qu'une compilation native réussira.

Remarque : `ku build --native hello.ku` **sans `-o` produit uniquement du code source C**, sans édition de liens. Utilisez la commande `--backend c` ci-dessus pour votre premier programme compilé.

## Créer un projet

Utilisez un modèle intégré lorsque vous avez besoin d'un répertoire de paquet :

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

Le point d'entrée du paquet est configuré dans `ku.mod`. `ku create --list` affiche les modèles disponibles. Il n'existe actuellement aucune commande `ku test` ou `ku fmt`.

Poursuivez avec le [guide du langage](/fr/docs/guide/language/), le [dépannage](/fr/docs/guide/errors/) et la [prise en charge des backends](/fr/docs/support/). Les sorties présentées sont des exemples attendus et ne remplacent pas la vérification de l'installation sur votre plateforme.
