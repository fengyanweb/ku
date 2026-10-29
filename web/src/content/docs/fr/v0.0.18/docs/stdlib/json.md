---
title: 'Référence JSON'
description: "Encodage et décodage JSON dans Ku, conversion KuValue, codes d'erreur et prise en charge des backends."
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
slug: fr/v0.0.18/docs/stdlib/json
---

`std.json` encode et décode JSON. `json.encode` emprunte l’entrée en lecture seule ; `json.decode` produit un `KuValue` propriétaire indépendant. Les deux renvoient Result. `stringify`, `parse` et `try_parse` restent compatibles.

## Importation et interfaces

```ku
import json from "std.json"
```

| Interface                    | Retour     | Signification                                                     |
| ---------------------------- | ---------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Encodage en lecture seule dans un nouveau texte JSON compact      |
| `json.parse(text:str)`       | `KuValue!` | Analyse stricte vers une valeur dynamique possédée indépendamment |
| `json.try_parse(text:str)`   | `KuValue!` | Alias de compatibilité de `parse`                                 |
| `json.encode(&value:any)`    | `str!`     | Alias de `stringify` inclus dans cette version                    |
| `json.decode(text:str)`      | `KuValue!` | Alias de `parse` inclus dans cette version                        |

`!` désigne Result. Utilisez `?` pour propager les erreurs ou `try/catch` pour les traiter. L'encodage ne consomme ni ne modifie l'entrée.

## Encoder et lire des valeurs

Exemple de base complet ; autres usages dans [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.encode(value)?
    println(text)

    decoded = json.decode(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

Les clés des objets sont triées de manière déterministe. Sortie attendue :

```text
{"count":7,"name":"Ku"}
Ku
8
```

Un `KuValue` analysé ne peut pas participer directement à un calcul entier. Convertissez-le d'abord avec `.as_int()?` ou `.as_str()?`. Un type incompatible renvoie une erreur. Les clés d'objet absentes renvoient également des erreurs plutôt que des valeurs par défaut automatiques.

## Données et limites

Les entrées prises en charge comprennent les chaînes, booléens, entiers, flottants finis, null, tableaux imbriqués, objets à clés textuelles et structs représentables. Les fonctions, Tasks, handles de ressources, Results, enums et cycles ne peuvent pas être encodés.

| Règle                   | Implémentation actuelle                                                                           |
| ----------------------- | ------------------------------------------------------------------------------------------------- |
| Syntaxe JSON            | Stricte ; refuse commentaires, virgules finales, contenu supplémentaire final et Unicode invalide |
| Entrée / sortie         | Chacune limitée à 1 000 000 d'octets                                                              |
| Budget de profondeur    | Racine à 0 ; refuse `depth > 32` ; 33 niveaux de conteneurs vides sont acceptés, 34 refusés       |
| Entiers                 | Signés sur 64 bits ; un dépassement est une erreur, sans conversion automatique en flottant       |
| Virgule flottante       | binary64 fini ; graphie d'origine, étiquettes de flottant et zéro négatif non conservés           |
| Unicode                 | Chinois, émojis, échappements, NUL intégré                                                        |
| Clés d'objet dupliquées | L'analyse conserve la dernière valeur                                                             |
| Ordre des clés          | L'encodage trie selon l'ordre des octets UTF-8                                                    |

`1.0` peut être encodé en `1`. Pour un flottant dont l’écriture entière dépasse int64, l’encodeur ajoute `.0` : `1e20` devient `100000000000000000000.0` et peut être redécodé. Les binary64 finis ne conservent ni l’écriture d’origine, ni le marqueur flottant, ni le signe du zéro négatif. Cette correction est incluse.

## Erreurs

Les erreurs récupérables utilisent le domaine `json`. Les codes courants sont `parse_error`, `stringify_error` et `out_of_memory`. Le `byte N` d'un message d'analyse est un décalage d'octets UTF-8 commençant à zéro. Consultez [erreurs et dépannage](/fr/v0.0.18/docs/guide/errors/) pour un gestionnaire minimal d'entrée invalide.

## Prise en charge des backends

L'interpréteur, le lanceur empaqueté par défaut et le backend C natif synchrone possèdent des implémentations JSON. **Le sous-ensemble natif Task restreint et LLVM ne prennent pas en charge JSON.** Les importations du lanceur par défaut dépendent toujours de leur emplacement source ; la prise en charge de JSON n'implique pas un paquet entièrement indépendant.

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md#127-json).
