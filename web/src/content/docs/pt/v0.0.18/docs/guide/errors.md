---
title: 'Erros e solução de problemas'
description: 'Diagnostique erros de instalação, importações, propriedade, JSON e recursos dos backends do Ku.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
    'docs/v0.0.18-r1.md',
  ]
slug: pt/v0.0.18/docs/guide/errors
---

Os diagnósticos do compilador podem mudar durante o desenvolvimento experimental. Ao relatar um problema, inclua a versão da CLI, o commit Git, o backend, o comando completo e um código-fonte mínimo.

## Identifique o executável do Ku

```sh
ku --version
ku -h
ku check hello.ku
```

Se `ku` não for encontrado, invoque o executável no diretório em que foi extraído. Use `ku.exe` no Windows. Se faltarem Rust ou bibliotecas de vinculação na compilação padrão, confira os [requisitos de instalação](/pt/v0.0.18/docs/install/) e mantenha a estrutura completa do arquivo compactado, em vez de copiar apenas a CLI.

## Leia os diagnósticos

A verificação normal informa o arquivo, a localização no código, um trecho do código e as orientações de correção disponíveis. As integrações com ferramentas podem usar:

```sh
ku check --json hello.ku
```

A saída está no formato **JSON Lines**, com um diagnóstico por linha. Os campos incluem `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes` e `helps`. O sucesso não produz saída; a ausência de JSON não significa que a verificação não foi executada.

| Diagnóstico | Causa comum                              | O que fazer                                                                         |
| ----------- | ---------------------------------------- | ----------------------------------------------------------------------------------- |
| `E0105`     | Uso de `let`                             | Declarar a variável pela primeira atribuição                                        |
| `E0302`     | Condição `if` / `while` que não é bool   | Escrever uma comparação booleana explícita                                          |
| `E0601`     | Módulo desconhecido da biblioteca padrão | Verificar o caminho real do módulo, como `std.json`                                 |
| `E0605`     | Importação explícita obrigatória ausente | Importar o módulo, por exemplo `import fs from "std.fs"`                            |
| `E0901`     | Leitura de um valor movido               | Clonar explicitamente um valor independente ou usar o empréstimo síncrono suportado |
| `E0913`     | Parâmetro async emprestado               | Usar um parâmetro com propriedade; clonar no chamador se o original for necessário  |
| `E0918`     | Escrever `&value` no ponto de chamada    | Passar um argumento comum; a assinatura declara o empréstimo                        |

Este é um resumo dos diagnósticos registrados, não uma lista de todos os erros. O registro oficial atual é [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/src/error.rs).

## Os erros de execução seguem um contrato separado

Os valores `domain/code` de Error em tempo de execução são separados dos códigos `E` do compilador. JSON inválido pode retornar `json/parse_error`; uma chave de objeto ausente pode retornar `object/missing_key`. Use `?` após uma chamada que pode falhar ou leia os seus campos em `try/catch`.

```ku
import json from "std.json"

fn main() {
    try {
        json.parse("[1,]")?
    } catch (error) {
        println(error.domain)
        println(error.code)
        println(error.message)
    }
}
```

Este programa didático provoca intencionalmente uma falha de análise recuperável. O `byte N` informado é um deslocamento de bytes UTF-8 com base zero. Não é uma contagem de caracteres em texto chinês ou emojis.

## Uma compilação nativa é rejeitada

Passar em `ku check` apenas comprova o sucesso da verificação do frontend. C nativo e LLVM têm limites próprios de recursos. Consulte a [matriz de suporte](/pt/v0.0.18/docs/support/) quando uma construção for explicitamente rejeitada. Aumentar um tempo limite ou obter sucesso com outro backend não verifica o caminho não suportado.

`ku build --native file.ku` sem `-o` apenas escreve C. Para compilar e vincular um executável, use `ku build --backend c file.ku` e instale a cadeia de ferramentas C para o destino.

Os erros de banco de dados `execution_unknown` e `execution_completed_without_result` não devem provocar repetição automática. Preserve as evidências da solicitação e da operação de negócio e determine primeiro o estado real da execução.

Para mais detalhes, consulte o [guia de diagnósticos do repositório](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/diagnostics.md).

Erros JSON mantêm domain/code entre backends, mas message pode diferir. ku run e runner podem acrescentar json parse failed:, enquanto C nativo não. Compare domain/code, não a mensagem inteira.
