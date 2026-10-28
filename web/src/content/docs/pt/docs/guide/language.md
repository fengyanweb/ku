---
title: 'Guia da linguagem'
description: 'Aprenda variáveis, funções, fluxo de controle, propriedade, módulos e erros recuperáveis do Ku.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
  ]
---

Esta documentação descreve o escopo congelado da pré-versão experimental v0.0.18. A publicação efetiva e os downloads estão registrados no GitHub Releases. Este guia apresenta a sintaxe essencial. O [contrato de sintaxe](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) e o [contrato semântico](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) do repositório continuam sendo as referências oficiais. São fragmentos didáticos; consulte as diferenças no [suporte dos backends](/pt/docs/support/).

## Variáveis e tipos

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

A primeira atribuição declara uma variável. Os tipos primitivos são `int`, `float`, `bool`, `str` e `null`. Os inteiros têm sinal e 64 bits; os números de ponto flutuante usam binary64. Os inteiros não viram condições por conversão implícita para um valor verdadeiro.

## Funções e fluxo de controle

Este exemplo básico completo vem de `examples/function.ku`:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Saída esperada: `30`. Este fragmento de fluxo de controle pode ser colocado dentro de `main`:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5` itera de `0` até `4`, inclusive. `if` e `while` exigem condições `bool`. Use chaves para várias instruções. As expressões de ramificação usam `match`; não existe `switch`.

## Propriedade e empréstimo somente para leitura

`int`, `float`, `bool` e `null` são valores Copy. Strings, arrays, objetos, structs, enums, Results e valores de função são Owned. Atribuições e argumentos comuns movem valores com propriedade por padrão. Use `.clone()` quando precisar de um valor independente. Tasks não podem ser clonados.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Uma função síncrona pode declarar um parâmetro emprestado somente para leitura:

```ku
fn length(&text: str): int {
    return text.len()
}
```

Chame-a como `length(value)`, sem `&` no argumento. Os dados emprestados não podem ser alterados, movidos para fora nem escapar da chamada. Combinações como parâmetros async emprestados continuam sem suporte. O empréstimo somente para leitura não garante a pureza de qualquer função.

## Structs, objetos e módulos

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Objetos dinâmicos usam `{ name: "Ku" }`. Um acesso comum a um índice de objeto ausente falha. `value["name"]?` trata a busca por meio de Result, retornando um `KuValue` no sucesso. Use `.as_str()?` ou `.as_int()?` para obter um tipo concreto. Uma chave ausente não se transforma silenciosamente em `null`.

As importações da biblioteca padrão usam caminhos com pontos, como `import json from "std.json"`. Os módulos locais podem usar `import math from "./math"`. As exportações de nível superior definidas pelo usuário começam com uma letra ASCII maiúscula; nomes em minúsculas permanecem privados ao arquivo.

## Erros recuperáveis

O tipo de retorno `T!` pode retornar T ou Error. `?` propaga um erro recuperável; `fail` cria um:

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

Error tem os campos `domain`, `code` e `message`. Um `catch` comum não captura `panic`, cancelamento de Task ou tempo limite. A limpeza de cancelamento em finally compartilha um orçamento com prazo absoluto; isso não garante a conclusão de qualquer limpeza definida pelo usuário.

## Limites de concorrência

O código da aplicação obtém um identificador Task de uso único ao chamar uma `async fn` e depois usa `await`. Não existem APIs de aplicação chamadas `task.spawn`, `Task.new` ou `thread.spawn`. O interpretador e o subconjunto Task nativo têm coberturas diferentes. Um exemplo que funciona em um backend não comprova suporte completo em outro. Consulte o [suporte de plataformas e backends](/pt/docs/support/).
