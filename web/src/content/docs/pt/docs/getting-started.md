---
title: 'Primeiros passos'
description: 'Escreva Hello Ku, verifique o código e diferencie interpretação de compilação.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'examples/hello.ku',
    'README.md',
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
---

Primeiro [instale o Ku](/pt/docs/install/) e confirme que `ku --version` funciona.

## Escreva o primeiro programa

Crie `hello.ku` com o mesmo conteúdo de `examples/hello.ku` no repositório:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` é o ponto de entrada. `println` escreve uma linha de texto. O Ku não usa `let` nem `let mut`.

## Verifique e execute

No diretório do arquivo:

```sh
ku check hello.ku
ku run hello.ku
```

`check` verifica erros léxicos, sintáticos e semânticos básicos sem executar o programa. A saída padrão esperada durante a execução é:

```text
Hello Ku
```

Se o Ku não estiver no `PATH`, invoque o executável usando o caminho completo da instalação. No Windows, o executável é `ku.exe`.

## Variáveis e funções

Este fragmento de sintaxe básica pode ser colocado dentro de `main`:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

A primeira atribuição declara uma variável. Um tipo explícito usa `name:type = value`. `print` escreve sem adicionar uma quebra de linha automática; `println` adiciona uma.

## Gere um executável

A página de instalação lista as ferramentas de compilação necessárias. Para o backend padrão:

```sh
ku build hello.ku
```

O backend padrão empacota um executor com interpretador. A compilação nativa em C exige um compilador C separado:

```sh
ku build --backend c hello.ku
```

A saída padrão fica em `.ku/build/<profile>/`; use o caminho exato informado pela CLI. Os dois backends cobrem recursos diferentes. Passar em `ku check` não comprova que uma compilação nativa terá sucesso.

Observação: `ku build --native hello.ku` **sem `-o` gera somente código-fonte C**, sem vinculação. Use o comando `--backend c` acima para o primeiro programa compilado.

## Crie um projeto

Use um modelo integrado quando precisar de um diretório de pacote:

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

A entrada do pacote é configurada em `ku.mod`. `ku create --list` lista os modelos disponíveis. Atualmente não existem comandos `ku test` nem `ku fmt`.

Continue com o [guia da linguagem](/pt/docs/guide/language/), a [solução de problemas](/pt/docs/guide/errors/) e o [suporte dos backends](/pt/docs/support/).
