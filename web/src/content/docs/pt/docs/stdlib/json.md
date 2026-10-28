---
title: 'Referência de JSON'
description: 'Codificação e decodificação JSON no Ku, conversão KuValue, códigos de erro e suporte dos backends.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.19.md',
  ]
---

> As atualizações de v0.0.18 abaixo estão por enquanto em inglês; o restante da tradução foi preservado. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## Importação e interfaces

```ku
import json from "std.json"
```

| Interface                    | Retorno    | Significado                                                       |
| ---------------------------- | ---------- | ----------------------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Codificação somente para leitura em novo texto JSON compacto      |
| `json.parse(text:str)`       | `KuValue!` | Análise estrita em um valor dinâmico com propriedade independente |
| `json.try_parse(text:str)`   | `KuValue!` | Alias de compatibilidade de `parse`                               |
| `json.encode(&value:any)`    | `str!`     | Alias de `stringify` no ramo de desenvolvimento                   |
| `json.decode(text:str)`      | `KuValue!` | Alias de `parse` no ramo de desenvolvimento                       |

`!` indica Result. Use `?` para propagar erros ou `try/catch` para tratá-los. A codificação não consome nem altera a entrada.

## Codifique e leia valores

Este é um exemplo básico completo. O ramo de desenvolvimento contém um exemplo de regressão mais abrangente em [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.19/examples/json.ku).

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

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## Erros

Os erros recuperáveis usam o domínio `json`. Os códigos comuns são `parse_error`, `stringify_error` e `out_of_memory`. O `byte N` em uma mensagem de análise é um deslocamento de bytes UTF-8 com base zero. Consulte [erros e solução de problemas](/pt/docs/guide/errors/) para um tratador mínimo de entrada inválida.

## Suporte dos backends

O interpretador, o executor empacotado padrão e o backend C nativo síncrono têm implementações de JSON. **O subconjunto Task nativo restrito e LLVM não suportam JSON.** As importações do executor padrão ainda dependem das localizações dos fontes; suporte a JSON não implica um pacote totalmente independente.

As assinaturas e os limites desta página foram conferidos no código e nos contratos. A verificação de execução deve indicar versão, commit e backend. Consulte o [contrato completo da sintaxe JSON](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md#127-json).
