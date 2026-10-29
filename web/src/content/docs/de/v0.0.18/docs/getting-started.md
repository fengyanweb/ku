---
title: 'Erste Schritte'
description: 'Schreiben Sie Hello Ku, prüfen Sie Ihren Quelltext und unterscheiden Sie Interpretation und Kompilierung.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'examples/hello.ku',
    'README.md',
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
slug: de/v0.0.18/docs/getting-started
---

[Installieren Sie zuerst Ku](/de/v0.0.18/docs/install/) und prüfen Sie, ob `ku --version` funktioniert.

## Ihr erstes Programm schreiben

Erstellen Sie `hello.ku` mit demselben Inhalt wie `examples/hello.ku` im Repository:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` ist der Einstiegspunkt. `println` gibt eine Textzeile aus. Ku verwendet weder `let` noch `let mut`.

## Prüfen und ausführen

Führen Sie im Verzeichnis der Datei Folgendes aus:

```sh
ku check hello.ku
ku run hello.ku
```

`check` prüft lexikalische, syntaktische und grundlegende semantische Fehler, ohne das Programm auszuführen. Bei der Ausführung wird folgende Standardausgabe erwartet:

```text
Hello Ku
```

Falls Ku nicht im `PATH` liegt, rufen Sie die ausführbare Datei über ihren vollständigen Installationspfad auf. Unter Windows heißt sie `ku.exe`.

## Variablen und Funktionen

Dieses grundlegende Syntaxfragment kann in `main` eingefügt werden:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

Die erste Zuweisung deklariert eine Variable. Ein expliziter Typ wird mit `name:type = value` angegeben. `print` gibt Text ohne automatischen Zeilenumbruch aus; `println` fügt einen hinzu.

## Eine ausführbare Datei erstellen

Die Installationsseite nennt die benötigten Build-Werkzeuge. Für das Standard-Backend:

```sh
ku build hello.ku
```

Das Standard-Backend verpackt einen Interpreter-Runner. Die native C-Kompilierung erfordert einen separaten C-Compiler:

```sh
ku build --backend c hello.ku
```

Die Standardausgabe liegt unter `.ku/build/<profile>/`; verwenden Sie den genauen von der CLI gemeldeten Pfad. Die beiden Backends unterstützen unterschiedliche Funktionen. Ein erfolgreiches `ku check` beweist nicht, dass ein nativer Build gelingt.

Hinweis: `ku build --native hello.ku` **gibt ohne `-o` nur C-Quelltext aus**, ohne zu linken. Verwenden Sie für Ihr erstes kompiliertes Programm den obigen Befehl `--backend c`.

## Ein Projekt erstellen

Wenn Sie ein Paketverzeichnis benötigen, verwenden Sie eine integrierte Vorlage:

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

Der Paketeinstieg wird in `ku.mod` konfiguriert. `ku create --list` listet verfügbare Vorlagen auf. Derzeit gibt es keine Befehle `ku test` oder `ku fmt`.

Lesen Sie anschließend den [Sprachleitfaden](/de/v0.0.18/docs/guide/language/), die [Fehlerbehebung](/de/v0.0.18/docs/guide/errors/) und die [Backend-Unterstützung](/de/v0.0.18/docs/support/).
