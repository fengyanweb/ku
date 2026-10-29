---
title: 'Fehler und Fehlerbehebung'
description: 'Diagnostizieren Sie Fehler bei Installation, Imports, Ownership, JSON und Backend-Fähigkeiten von Ku.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
    'docs/v0.0.18-r1.md',
  ]
---

Compilerdiagnosen können sich während der experimentellen Entwicklung ändern. Geben Sie bei Problemmeldungen CLI-Version, Git-Commit, Backend, vollständigen Befehl und minimalen Quelltext an.

## Die ausführbare Ku-Datei identifizieren

```sh
ku --version
ku -h
ku check hello.ku
```

Wenn `ku` nicht gefunden wird, rufen Sie die ausführbare Datei aus ihrem entpackten Verzeichnis auf. Unter Windows verwenden Sie `ku.exe`. Fehlen dem Standard-Build Rust oder Bibliotheken zum Linken, prüfen Sie die [Installationsvoraussetzungen](/de/docs/install/) und behalten Sie die vollständige Archivstruktur bei, statt nur die CLI zu kopieren.

## Diagnosen lesen

Die normale Prüfung meldet Datei, Quelltextposition, Quelltextauszug und verfügbare Korrekturhinweise. Werkzeugintegrationen können Folgendes verwenden:

```sh
ku check --json hello.ku
```

Die Ausgabe besteht aus **JSON Lines**, mit einer Diagnose pro Zeile. Zu den Feldern gehören `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes` und `helps`. Bei Erfolg erfolgt keine Ausgabe; fehlende JSON-Ausgabe bedeutet nicht, dass die Prüfung nicht ausgeführt wurde.

| Diagnose | Häufige Ursache                            | Vorgehen                                                                                  |
| -------- | ------------------------------------------ | ----------------------------------------------------------------------------------------- |
| `E0105`  | Verwendung von `let`                       | Die Variable durch ihre erste Zuweisung deklarieren                                       |
| `E0302`  | Eine `if`-/`while`-Bedingung ohne bool-Typ | Einen expliziten booleschen Vergleich schreiben                                           |
| `E0601`  | Unbekanntes Standardbibliotheksmodul       | Den tatsächlichen Modulpfad prüfen, etwa `std.json`                                       |
| `E0605`  | Erforderlicher expliziter Import fehlt     | Das Modul importieren, zum Beispiel `import fs from "std.fs"`                             |
| `E0901`  | Lesen eines verschobenen Werts             | Einen unabhängigen Wert explizit klonen oder unterstütztes synchrones Ausleihen verwenden |
| `E0913`  | Ausgeliehener async-Parameter              | Einen eigenen Parameter verwenden; beim Aufrufer klonen, falls das Original benötigt wird |
| `E0918`  | `&value` an der Aufrufstelle               | Ein gewöhnliches Argument übergeben; die Signatur deklariert das Ausleihen                |

Dies fasst registrierte Diagnosen zusammen und führt nicht jeden Fehler auf. Das aktuell maßgebliche Register ist [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/error.rs).

## Laufzeitfehler folgen einem separaten Vertrag

Die Laufzeitwerte von Error `domain/code` sind von den `E`-Codes des Compilers getrennt. Ungültiges JSON kann `json/parse_error` zurückgeben; ein fehlender Objektschlüssel kann `object/missing_key` zurückgeben. Verwenden Sie nach einem fehlbaren Aufruf `?` oder lesen Sie seine Felder in `try/catch`.

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

Dieses Lernprogramm löst absichtlich einen behebbaren Parsefehler aus. Ein gemeldetes `byte N` ist ein nullbasierter UTF-8-Byteoffset. Es handelt sich nicht um eine Zeichenanzahl bei chinesischem Text oder Emojis.

## Ein nativer Build wird abgelehnt

Ein erfolgreiches `ku check` belegt nur, dass die Frontend-Prüfung erfolgreich war. Native C und LLVM haben jeweils begrenzte Fähigkeiten. Prüfen Sie die [Unterstützungsmatrix](/de/docs/support/), wenn ein Konstrukt ausdrücklich abgelehnt wird. Ein höheres Timeout oder Erfolg mit einem anderen Backend verifiziert den nicht unterstützten Pfad nicht.

`ku build --native file.ku` ohne `-o` schreibt nur C. Zum Kompilieren und Linken einer ausführbaren Datei verwenden Sie `ku build --backend c file.ku` und installieren die C-Toolchain für das Zielsystem.

Die Datenbankfehler `execution_unknown` und `execution_completed_without_result` dürfen keine automatische Wiederholung auslösen. Bewahren Sie Anfrage- und Geschäftsnachweise auf und klären Sie zuerst den tatsächlichen Ausführungszustand.

Weitere Einzelheiten finden Sie im [Diagnoseleitfaden des Repositorys](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/diagnostics.md).

Bei JSON-Fehlern stimmen domain/code zwischen den Backends überein, die genaue message kann abweichen. ku run und der Standard-Runner können den Präfix json parse failed: ausgeben, natives C nicht. Prüfen Sie domain/code statt vollständiger Meldungstexte.
