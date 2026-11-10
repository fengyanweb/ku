---
title: 'Bibliothèque standard'
description: 'Trouvez les modules de la bibliothèque standard Ku, leurs signatures essentielles et les limites des backends.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
    'docs/v0.0.18-r1.md',
  ]
slug: fr/v0.0.18/docs/stdlib
---

La [référence de syntaxe](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) décrit la bibliothèque standard. Consultez la [matrice de prise en charge](/fr/v0.0.18/docs/support/) pour les limites des backends.

## Importer des modules

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes` et `net` exigent des importations explicites. Certains modules historiques restent directement appelables ; les nouveaux exemples privilégient les importations explicites pour clarifier les dépendances. `std.task` est un espace de noms interne d'observation et de tests de charge, pas une API de concurrence applicative.

| Module                               | Rôle                                                       | Limites à retenir                                                                                                         |
| ------------------------------------ | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `std.string` / méthodes de chaîne    | Longueur Unicode, longueur en octets, recherche, découpage | `trim/lower/upper` natifs n'ont pas d'implémentation Unicode                                                              |
| `std.array` / méthodes de tableau    | Longueur, recherche, nouveaux tableaux, map                | L'échec d'un index direct diffère de `try_get`, récupérable                                                               |
| `std.object`                         | Recherche avec repli explicite via `get_or`                | Un index ordinaire absent ne vaut pas null par défaut                                                                     |
| [std.json](/fr/v0.0.18/docs/stdlib/json/)    | Encodage et décodage JSON stricts                          | Non pris en charge dans les Tasks natives et LLVM                                                                         |
| `std.fs`                             | Fichiers, vérification d'existence, contenu de répertoires | Les chemins relatifs partent du répertoire du source `.ku` courant                                                        |
| `std.time`                           | Heure, dates, durées, sommeil                              | Les attentes synchrones et Task suivent des chemins d'exécution différents                                                |
| `std.http`                           | Clients HTTP, services, réponses                           | Les différences de backend et de cycle de vie subsistent ; ce n'est pas une implémentation réseau Task générique complète |
| `std.config`                         | Environnement et configuration                             | Ne pas versionner de véritables identifiants dans le code                                                                 |
| `std.pg` / `std.redis` / `std.mysql` | Clients unifiés avec pools bornés                          | Natifs uniquement ; validation sur serveur réel et limites TLS à vérifier séparément                                      |
| `std.bytes` / `std.net`              | Données binaires, client TCP, TLS facultatif               | Chemin C natif généré ; TLS exige un paquet adapté à la cible                                                             |
| `std.lexer` / `std.parser`           | Aides au débogage du compilateur                           | Renvoient des résumés de débogage, pas des structures publiques Token / AST complètes                                     |

## Signatures courantes

Il s'agit de signatures documentaires. `&` désigne un mode de paramètre en lecture seule pris en charge ; `T!` désigne un résultat récupérable :

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

`"中😀".len()` vaut 2 et `.byte_len()` vaut 7. La longueur compte les valeurs scalaires Unicode ; le découpage utilise des indices de caractères et non des octets UTF-8. `array.push` et `array.concat` renvoient de nouveaux tableaux sans modifier leurs entrées.

## Lire un fichier

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Créez d'abord un fichier UTF-8 `message.txt` à côté du source. Un fichier absent ou un échec de lecture renvoie une erreur. Cet exemple dépend de ce fichier d'entrée. `try_read` et `try_write` restent des alias compatibles ayant les mêmes types de résultat.

`read_dir` renvoie des chemins triés possédés indépendamment, dans la limite de 10 000 entrées et de 1 000 000 d'octets de chemins au total. Les opérations de fichiers du sous-ensemble natif Task restreint utilisent un pool fixe en arrière-plan. Cela n'établit pas une attente réseau générique via le même mécanisme.
