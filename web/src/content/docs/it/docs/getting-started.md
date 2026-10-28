---
title: 'Primi passi'
description: 'Scrivi Hello Ku, controlla il codice e distingui interpretazione e compilazione.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources: ['examples/hello.ku', 'README.md', 'docs/syntax.md', 'docs/v0.0.19.md']
---

Questa documentazione descrive l’ambito fissato della versione preliminare sperimentale v0.0.18. La pubblicazione effettiva e i download sono registrati in GitHub Releases. Questa pagina usa sintassi di base e comandi CLI già presenti in v0.0.17. Per prima cosa [installa Ku](/it/docs/install/) e verifica che `ku --version` funzioni.

## Scrivi il tuo primo programma

Crea `hello.ku` con lo stesso contenuto di `examples/hello.ku` nel repository:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` è il punto di ingresso. `println` scrive una riga di testo. Ku non usa `let` né `let mut`.

## Controlla ed esegui

Dalla directory del file:

```sh
ku check hello.ku
ku run hello.ku
```

`check` controlla errori lessicali, sintattici e semantici di base senza eseguire il programma. L'output standard previsto durante l'esecuzione è:

```text
Hello Ku
```

Se Ku non è in `PATH`, richiama l'eseguibile usando il percorso di installazione completo. Su Windows, l'eseguibile è `ku.exe`.

## Variabili e funzioni

Questo frammento di sintassi di base può essere inserito in `main`:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

La prima assegnazione dichiara una variabile. Un tipo esplicito si indica con `name:type = value`. `print` scrive senza andare a capo automaticamente; `println` aggiunge un ritorno a capo.

## Compila un eseguibile

La pagina di installazione elenca gli strumenti di compilazione necessari. Per il backend predefinito:

```sh
ku build hello.ku
```

Il backend predefinito impacchetta un runner con interprete. La compilazione nativa in C richiede un compilatore C separato:

```sh
ku build --backend c hello.ku
```

L'output predefinito si trova in `.ku/build/<profile>/`; usa il percorso esatto riportato dalla CLI. I due backend coprono funzionalità diverse. Superare `ku check` non dimostra che la compilazione nativa riuscirà.

Nota: `ku build --native hello.ku` **senza `-o` emette solo codice sorgente C**, senza collegarlo. Per il primo programma compilato usa il comando `--backend c` riportato sopra.

## Crea un progetto

Usa un modello integrato quando hai bisogno di una directory di pacchetto:

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

Il punto di ingresso del pacchetto si configura in `ku.mod`. `ku create --list` elenca i modelli disponibili. Attualmente non esistono comandi `ku test` o `ku fmt`.

Continua con la [guida al linguaggio](/it/docs/guide/language/), la [risoluzione dei problemi](/it/docs/guide/errors/) e il [supporto dei backend](/it/docs/support/). Gli output mostrati sono esempi attesi e non sostituiscono la verifica dell'installazione sulla tua piattaforma.
