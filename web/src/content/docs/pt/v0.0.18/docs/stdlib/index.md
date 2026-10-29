---
title: 'Biblioteca padrão'
description: 'Encontre módulos da biblioteca padrão do Ku, assinaturas essenciais e limites dos backends.'
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
slug: pt/v0.0.18/docs/stdlib
---

A [referência de sintaxe](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) descreve a biblioteca padrão. Consulte a [matriz de suporte](/pt/v0.0.18/docs/support/) para os limites dos backends.

## Importe módulos

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes` e `net` exigem importações explícitas. Alguns módulos históricos ainda podem ser chamados diretamente; os novos exemplos preferem importações explícitas para esclarecer as dependências. `std.task` é um espaço de nomes interno para observação e testes de estresse, não uma API de concorrência para aplicações.

| Módulo                               | Finalidade                                                  | Limites a lembrar                                                                                              |
| ------------------------------------ | ----------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| `std.string` / métodos de string     | Comprimento Unicode, comprimento em bytes, busca, recortes  | `trim/lower/upper` nativos não têm implementação Unicode                                                       |
| `std.array` / métodos de array       | Comprimento, busca, novos arrays, map                       | A falha de um índice direto difere do `try_get` recuperável                                                    |
| `std.object`                         | Busca com alternativa explícita via `get_or`                | Um índice comum ausente não assume null por padrão                                                             |
| [std.json](/pt/v0.0.18/docs/stdlib/json/)    | Codificação e decodificação estritas de JSON                | Sem suporte em Tasks nativos e LLVM                                                                            |
| `std.fs`                             | Arquivos, verificação de existência, listagem de diretórios | Caminhos relativos são resolvidos a partir do diretório do código `.ku` atual                                  |
| `std.time`                           | Hora, datas, durações, pausas                               | Esperas síncronas e de Tasks usam caminhos de execução diferentes                                              |
| `std.http`                           | Clientes HTTP, serviços, respostas                          | Permanecem diferenças de backend e ciclo de vida; não é uma implementação completa de rede genérica para Tasks |
| `std.config`                         | Ambiente e configuração                                     | Não versionar credenciais reais no código                                                                      |
| `std.pg` / `std.redis` / `std.mysql` | Clientes unificados com pools limitados                     | Somente nativos; validação em servidores reais e limites TLS exigem verificações separadas                     |
| `std.bytes` / `std.net`              | Dados binários, cliente TCP, TLS opcional                   | Caminho C nativo gerado; TLS exige um pacote correspondente ao destino                                         |
| `std.lexer` / `std.parser`           | Auxiliares de depuração do compilador                       | Retornam resumos de depuração, não estruturas públicas completas de Token / AST                                |

## Assinaturas comuns

Estas são assinaturas de documentação. `&` indica um modo de parâmetro somente para leitura suportado; `T!` indica um resultado recuperável:

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

`"中😀".len()` é 2 e `.byte_len()` é 7. O comprimento conta valores escalares Unicode; os recortes usam índices de caracteres, não bytes UTF-8. `array.push` e `array.concat` retornam novos arrays sem alterar suas entradas.

## Leia um arquivo

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Primeiro crie um `message.txt` em UTF-8 ao lado do código-fonte. Um arquivo ausente ou uma leitura malsucedida retorna um erro. Este exemplo depende desse arquivo de entrada. `try_read` e `try_write` continuam sendo aliases compatíveis com os mesmos tipos de resultado.

`read_dir` retorna caminhos ordenados com propriedade independente, limitados a 10.000 entradas e 1.000.000 de bytes de caminho no total. As operações de arquivo do subconjunto Task nativo restrito usam um pool fixo em segundo plano. Isso não comprova esperas de rede genéricas pelo mesmo mecanismo.
