---
title: 'Référence JSON'
description: "Encodage et décodage JSON dans Ku, conversion KuValue, codes d'erreur et prise en charge des backends."
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
  ]
---

> Les compléments v0.0.18 ci-dessous sont actuellement en anglais ; le reste de la traduction est conservé. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## Importation et interfaces

```ku
import json from "std.json"
```

| Interface                    | Retour     | Signification                                                     |
| ---------------------------- | ---------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Encodage en lecture seule dans un nouveau texte JSON compact      |
| `json.parse(text:str)`       | `KuValue!` | Analyse stricte vers une valeur dynamique possédée indépendamment |
| `json.try_parse(text:str)`   | `KuValue!` | Alias de compatibilité de `parse`                                 |
| `json.encode(&value:any)`    | `str!`     | Alias de `stringify` dans la branche de développement             |
| `json.decode(text:str)`      | `KuValue!` | Alias de `parse` dans la branche de développement                 |

`!` désigne Result. Utilisez `?` pour propager les erreurs ou `try/catch` pour les traiter. L'encodage ne consomme ni ne modifie l'entrée.

## Encoder et lire des valeurs

Voici un exemple élémentaire complet. La branche de développement contient un exemple de régression plus large dans [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
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

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## Erreurs

Les erreurs récupérables utilisent le domaine `json`. Les codes courants sont `parse_error`, `stringify_error` et `out_of_memory`. Le `byte N` d'un message d'analyse est un décalage d'octets UTF-8 commençant à zéro. Consultez [erreurs et dépannage](/fr/docs/guide/errors/) pour un gestionnaire minimal d'entrée invalide.

## Prise en charge des backends

L'interpréteur, le lanceur empaqueté par défaut et le backend C natif synchrone possèdent des implémentations JSON. **Le sous-ensemble natif Task restreint et LLVM ne prennent pas en charge JSON.** Les importations du lanceur par défaut dépendent toujours de leur emplacement source ; la prise en charge de JSON n'implique pas un paquet entièrement indépendant.

Les signatures et limites de cette page ont été vérifiées dans les sources et les contrats. Toute vérification d'exécution doit nommer la version, le commit et le backend. Voir le [contrat syntaxique JSON complet](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md#127-json).
