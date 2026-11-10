---
title: 'Referência de JSON'
description: 'Codificação e decodificação JSON no Ku, conversão KuValue, códigos de erro e suporte dos backends.'
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
slug: pt/v0.0.18/docs/stdlib/json
---

`std.json` codifica e decodifica JSON. `json.encode` empresta a entrada somente para leitura; `json.decode` cria um `KuValue` com propriedade independente. Ambos retornam Result. `stringify`, `parse` e `try_parse` continuam compatíveis.

## Importação e interfaces

```ku
import json from "std.json"
```

| Interface                    | Retorno    | Significado                                                       |
| ---------------------------- | ---------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Codificação somente para leitura em novo texto JSON compacto      |
| `json.parse(text:str)`       | `KuValue!` | Análise estrita em um valor dinâmico com propriedade independente |
| `json.try_parse(text:str)`   | `KuValue!` | Alias de compatibilidade de `parse`                               |
| `json.encode(&value:any)`    | `str!`     | Alias de `stringify` incluído nesta versão                        |
| `json.decode(text:str)`      | `KuValue!` | Alias de `parse` incluído nesta versão                            |

`!` indica Result. Use `?` para propagar erros ou `try/catch` para tratá-los. A codificação não consome nem altera a entrada.

## Codifique e leia valores

Exemplo básico completo; mais usos em [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/examples/json.ku).

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

As chaves dos objetos são ordenadas de forma determinística. Saída esperada:

```text
{"count":7,"name":"Ku"}
Ku
8
```

Um `KuValue` analisado não pode participar diretamente de aritmética de inteiros. Converta primeiro com `.as_int()?` ou `.as_str()?`. Um tipo incompatível retorna um erro. Chaves de objeto ausentes também retornam erros, em vez de valores padrão automáticos.

## Dados e limites

As entradas suportadas incluem strings, booleanos, inteiros, números finitos de ponto flutuante, null, arrays aninhados, objetos com chaves de string e structs representáveis. Funções, Tasks, identificadores de recursos, Results, enums e ciclos não podem ser codificados.

| Regra                       | Implementação atual                                                                                  |
| --------------------------- | ---------------------------------------------------------------------------------------------------- |
| Sintaxe JSON                | Estrita; rejeita comentários, vírgulas finais, conteúdo adicional ao final e Unicode inválido        |
| Entrada / saída             | Cada uma limitada a 1.000.000 de bytes                                                               |
| Limite de profundidade      | A raiz tem profundidade 0; rejeita `depth > 32`; aceita 33 níveis de contêineres vazios e rejeita 34 |
| Inteiros                    | Com sinal de 64 bits; estouro é um erro, sem conversão automática para ponto flutuante               |
| Ponto flutuante             | binary64 finito; representação original, etiquetas de float e zero negativo não são preservados      |
| Unicode                     | Chinês, emojis, escapes, NUL incorporado                                                             |
| Chaves de objeto duplicadas | A análise conserva o último valor                                                                    |
| Ordem das chaves            | A codificação ordena pela ordem de bytes UTF-8                                                       |

`1.0` pode ser codificado como `1`. Se o texto inteiro de um float ultrapassar int64, o codificador acrescenta `.0`: `1e20` vira `100000000000000000000.0` e pode ser decodificado novamente. Valores binary64 finitos não preservam grafia original, marca float ou sinal do zero negativo. A correção está incluída.

## Erros

Os erros recuperáveis usam o domínio `json`. Os códigos comuns são `parse_error`, `stringify_error` e `out_of_memory`. O `byte N` em uma mensagem de análise é um deslocamento de bytes UTF-8 com base zero. Consulte [erros e solução de problemas](/pt/v0.0.18/docs/guide/errors/) para um tratador mínimo de entrada inválida.

## Suporte dos backends

O interpretador, o executor empacotado padrão e o backend C nativo síncrono têm implementações de JSON. **O subconjunto Task nativo restrito e LLVM não suportam JSON.** As importações do executor padrão ainda dependem das localizações dos fontes; suporte a JSON não implica um pacote totalmente independente.

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md#127-json).
