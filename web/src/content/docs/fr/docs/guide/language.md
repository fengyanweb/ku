---
title: 'Guide du langage'
description: 'Découvrez les variables, fonctions, flux de contrôle, règles de propriété, modules et erreurs récupérables de Ku.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
    'docs/v0.0.18-r1.md',
  ]
---

Ce guide présente la syntaxe essentielle. Le [référence syntaxique](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) et le [référence sémantique](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) du dépôt restent les références faisant autorité. Ces fragments sont pédagogiques ; consultez la [prise en charge des backends](/fr/docs/support/) pour les différences.

## Variables et types

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

La première affectation déclare une variable. Les types primitifs sont `int`, `float`, `bool`, `str` et `null`. Les entiers sont signés sur 64 bits ; les flottants utilisent binary64. Les entiers ne deviennent pas des conditions par conversion implicite en valeur vraie.

## Fonctions et flux de contrôle

Cet exemple élémentaire complet provient de `examples/function.ku` :

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Sortie attendue : `30`. Ce fragment de contrôle peut être placé dans `main` :

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` parcourt les valeurs de `0` à `4`. `if` et `while` exigent des conditions de type `bool`. Utilisez des accolades pour plusieurs instructions. Les expressions de branchement utilisent `match` ; il n'existe pas de `switch`.

<a id="ownership"></a>

## Propriété et emprunts en lecture seule

Les valeurs Owned sont déplacées par défaut et libérées de façon déterministe à la fin de leur portée propriétaire. Une copie indépendante demande `.clone()`. Ku n’introduit pas de GC global par traçage ; des structures internes partagées, notamment les environnements de fermeture, peuvent utiliser le comptage de références et avoir un coût d’exécution. L’emprunt synchrone en lecture seule est déclaré par `&name: T`. Un paramètre Owned ordinaire peut déplacer la valeur même s’il ne fait que la lire. L’appel reste `length(value)`, sans `&value` ; les emprunts async ne sont pas pris en charge.

`int`, `float`, `bool` et `null` sont des valeurs Copy. Les chaînes, tableaux, objets, structs, enums, Results et valeurs de fonction sont Owned. Les affectations et les arguments ordinaires déplacent les valeurs possédées par défaut. Utilisez `.clone()` pour obtenir une valeur indépendante. Les Tasks ne peuvent pas être clonées.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Une fonction synchrone peut déclarer un paramètre emprunté en lecture seule :

```ku
fn length(&text: str): int {
    return text.len()
}
```

Appelez-la sous la forme `length(value)`, sans `&` sur l'argument. Les données empruntées ne peuvent être modifiées, déplacées hors de l'emprunt ni s'échapper de l'appel. Les combinaisons telles que les paramètres async empruntés restent non prises en charge. L'emprunt en lecture seule ne garantit pas la pureté de toute fonction.

## Structs, objets et modules

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Les objets dynamiques utilisent `{ name: "Ku" }`. Un accès ordinaire à un index d'objet absent échoue. `value["name"]?` effectue la recherche via Result et fournit un `KuValue` en cas de réussite. Utilisez `.as_str()?` ou `.as_int()?` pour obtenir un type concret. Une clé absente ne devient pas silencieusement `null`.

Les importations de la bibliothèque standard utilisent des chemins à points, comme `import json from "std.json"`. Les modules locaux peuvent utiliser `import math from "./math"`. Les exportations de premier niveau définies par l'utilisateur commencent par une majuscule ASCII ; les noms en minuscules restent privés au fichier.

## Erreurs récupérables

Le type de retour `T!` peut renvoyer T ou Error. `?` propage une erreur récupérable ; `fail` en crée une :

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    try {
        println(read_name()?)
    } catch (error) {
        println(error.message)
    }
}
```

Error possède les champs `domain`, `code` et `message`. Un `catch` ordinaire n'intercepte ni `panic`, ni l'annulation de Task, ni l'expiration d'un délai. Le nettoyage d'annulation dans finally partage un budget à échéance absolue ; il ne garantit pas l'achèvement de n'importe quel nettoyage utilisateur.

## Limites de concurrence

Le code applicatif obtient un handle Task à usage unique en appelant une `async fn`, puis utilise `await`. Il n'existe aucune API applicative nommée `task.spawn`, `Task.new` ou `thread.spawn`. L'interpréteur et le sous-ensemble natif Task ont des couvertures différentes. Un exemple fonctionnant sur un backend ne prouve pas la prise en charge complète sur un autre. Voir la [prise en charge des plateformes et backends](/fr/docs/support/).
