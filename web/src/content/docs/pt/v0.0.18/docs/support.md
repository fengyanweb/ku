---
title: 'Suporte de plataformas e backends'
description: 'Diferencie plataformas publicadas do Ku, interpretador, executor, C nativo, subconjunto Task e suporte LLVM.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
    'docs/v0.0.18-r1.md',
  ]
slug: pt/v0.0.18/docs/support
---

## Plataformas CLI publicadas

A versão preliminar experimental v0.0.18 cobre Windows x64 (MSVC), Linux x64 (GNU) e macOS arm64 (Apple Silicon). Cada plataforma tem seu próprio arquivo. Não existe um único executável para os três sistemas. Não foram verificados arquivos de outras plataformas; consulte a [instalação](/pt/v0.0.18/docs/install/).

## Caminhos de execução

| Capacidade                                                   | Interpretador `ku run`               | Executor padrão `ku build`                | C nativo síncrono                                       | Task nativo (subconjunto)                         | Protótipo textual LLVM      |
| ------------------------------------------------------------ | ------------------------------------ | ----------------------------------------- | ------------------------------------------------------- | ------------------------------------------------- | --------------------------- |
| Linguagem básica, funções, controle                          | Implementados                        | Caminho do interpretador                  | Subconjunto implementado                                | Subconjunto tipado restrito                       | Protótipo limitado          |
| JSON                                                         | Suportado                            | Suportado                                 | Suportado                                               | Não suportado                                     | Não suportado               |
| Grafo completo de importações locais independente dos fontes | Carrega fontes                       | Não suportado; incorpora apenas a entrada | Expandido durante a geração                             | Expandido no subconjunto suportado                | Sem promessa de implantação |
| async / await                                                | Implementado com limites de recursos | Caminho do interpretador                  | Subconjunto específico ou rejeição                      | Parcial, sem async completo                       | Sem suporte completo        |
| Clientes PostgreSQL / Redis / MySQL                          | Sem acesso a bancos                  | Sem acesso interpretado a bancos          | Somente nativos                                         | Sem espera Task genérica                          | Não suportados              |
| Cliente TCP / TLS                                            | Sem caminho nativo correspondente    | Sem caminho nativo correspondente         | Implementação C gerada; TLS exige pacote correspondente | Sem afirmação de espera de rede genérica completa | Não suportado               |

As compilações padrão exigem a cadeia Rust correspondente e o vinculador. C nativo exige uma cadeia C para o destino. Passar em `ku check` não comprova que nenhuma das duas compilações funcione. Um comando LLVM não implica um backend de publicação completo.

## O subconjunto Task nativo

Esta versão inclui o caminho C nativo limitado `async fn main(): null!`: primitivas listadas, strings, arrays planos, Results de um nível, início/movimento/await de Task, parte do controle de fluxo e operações limitadas de arquivos/diretórios em segundo plano. TCP/TLS local usa um pool bloqueante fixo. Isso não equivale a readiness de rede geral ou M:N completo; consulte a referência de concorrência da versão.

Continuam incompletos a espera de rede Task genérica, a execução M:N completa, as combinações arbitrárias de heap dinâmico, todas as combinações de capturas/funções async e os testes prolongados completos. Consulte os subconjuntos exatos aceitos e rejeitados no [contrato de concorrência](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md).

## Limites de bancos de dados e rede

As APIs de banco de dados usam `module.client(config)?`, seguido de métodos do receptor, com um pool limitado dentro do cliente. A validação em servidor real é separada para cada driver, sistema e commit. Uma ABI simulada ou uma vinculação bem-sucedida não substitui uma consulta real.

Redis usa TCP sem criptografia. MySQL não tem uma verificação uniforme de certificados que feche a conexão em caso de erro entre Oracle/MariaDB. Portanto, conexões diretas devem usar loopback, redes privadas controladas ou um túnel TLS verificado. PostgreSQL remoto exige `sslmode=verify-full` explícito e certificados raiz confiáveis. TLS nativo em `std.net` não significa que HTTP, Redis ou o interpretador reutilizem esse caminho.

## Auto-hospedagem e compatibilidade

O analisador léxico e o parser auto-hospedados do Ku têm subconjuntos limitados e verificações diferenciais. Um parser, verificador, IR e backend totalmente auto-hospedados continuam incompletos; Rust ainda fornece a cadeia de bootstrap. A versão 0.0.x não promete compatibilidade do código-fonte nem da ABI C/FFI gerada.

## Escopo e limites de v0.0.18

- JSON: `encode`/`decode` compartilham `stringify`/`parse`, incluindo a correção de ida e volta de `1e20`.
- Task nativo mantém o subconjunto limitado listado; admissão no encerramento e limpeza não equivalem a M:N completo ou suspensão de rede geral.
- HTTP oferece readiness de sockets, filas, respostas e keep-alive limitados e diagnósticos limitados opcionais; isso não conclui toda aceitação de carga.
- `ku supervise` controla workers de programas completos, admissão, bytes das filas, saída e limpeza; não um worker por requisição ou Task nem aplicação automática a `ku run`.

**Problema HTTP conhecido:** podem ocorrer erros HTTP 504 intermitentes. A causa ainda não foi confirmada.

### Limites de memória de processos

| Limites de memória de processos | `ku supervise`                                                                                                                                                                                                                                      |
| ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux                           | Memória contabilizada pelo cgroup v2; exige raiz de domínio delegada e gravável com `memory`, `pids`, `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` e `cgroup.kill`. Falta de controles ou permissões impede iniciar a aplicação. |
| Windows                         | Job Objects aninhados limitam workers e grupo por memória virtual comprometida, não RSS. O compromisso inicial é verificado antes de liberar a thread suspensa; a política do host pode recusar a associação ao Job.                                |
| macOS                           | Esse isolamento não é nativo e é recusado antes de iniciar a aplicação. Use host ou VM Linux devidamente configurado; não há encaminhamento automático para VM.                                                                                     |

O domínio do worker contabiliza alocações Ku e de bibliotecas externas. cgroup Linux e memória comprometida Windows são medidas distintas. `memory.max` pode permitir excesso transitório; não há teto absoluto de toda memória física em cada instante.

O coordenador não tem limite rígido de memória do SO; reservas e admissão não constituem RSS preciso. O heap completo de cada Task não é medido exatamente, nem pools, caches e bibliotecas são atribuíveis com precisão. Bancos externos, host/VM, compiladores e `ku run` comum não estão automaticamente sujeitos ao limite do worker.

Se o SO terminar um worker, o coordenador informa falha e só devolve reservas após limpeza confirmada. Algumas alocações falham antes, mas esgotamento em bibliotecas externas não gera necessariamente Result comum. **Operações de banco nunca são repetidas automaticamente**; saída do worker não prova rollback e os efeitos podem ser desconhecidos.

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
