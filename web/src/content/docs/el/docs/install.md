---
title: 'Εγκατάσταση και λήψη'
description: 'Κατεβάστε την πειραματική προέκδοση Ku 0.0.19 και κατανοήστε τις απαιτήσεις πλατφόρμας και κατασκευής.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-21'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

> Οι νέες πληροφορίες για τη v0.0.19 ακολουθούν προς το παρόν στα αγγλικά· η υπόλοιπη μετάφραση διατηρείται. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

This page targets the **v0.0.19 experimental prerelease**. Use the actual [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) and its assets; documentation alone does not establish completed publication or production readiness.

## Επιλέξτε πλατφόρμα

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19). `native_ci-*` artifacts are CI examples, not CLI installers.

| Σύστημα | Αρχιτεκτονική         | Αρχείο                                                                                                                                                |
| ------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / arm64 | [`ku-v0.0.19-aarch64-apple-darwin.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)         |
| Windows | x64 / MSVC            | [`ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz)     |
| Linux   | x64 / GNU             | [`ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz`](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) |

[Download checksums: SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS)

Δεν έχουν επαληθευτεί αρχεία για άλλες πλατφόρμες. Η υποστήριξη διασταυρούμενης μεταγλώττισης δεν σημαίνει ότι υπάρχει δημοσιευμένο πακέτο για κάθε λειτουργικό σύστημα ή αρχιτεκτονική.

## Αποσυμπίεση και επαλήθευση

1. Επαληθεύστε το αρχείο με το `SHA256SUMS` της έκδοσης. Ένα ξεχωριστό `.manifest.json` περιγράφει την ακεραιότητα του συμπιεσμένου αρχείου και των περιεχομένων του. Τα αθροίσματα ελέγχου ελέγχουν ακεραιότητα· δεν είναι υπογραφές του εκδότη.
2. Αποσυμπιέστε **ολόκληρο το αρχείο** σε κενό κατάλογο και διαβάστε το `README-INSTALL.md`. Διατηρήστε τη σχετική διάταξη των `ku`, `libku.rlib`, `deps/` και `native-tls/`.
3. Ελέγξτε την έκδοση από εκείνον τον κατάλογο. Σε macOS / Linux:

```sh
./ku --version
```

Σε Windows PowerShell:

```powershell
.\ku.exe --version
```

Η αναφερόμενη έκδοση πρέπει να είναι `0.0.19`. Προαιρετικά προσθέστε τον κατάλογο στο `PATH` και εκτελέστε `ku --version`. Εγκαταστήστε το παρεχόμενο `ku-language-0.0.19.vsix` για ενσωμάτωση με το VS Code.

## Interpreter and VS Code extension

Each platform archive above includes the Ku CLI (`ku run` uses the interpreter) and `ku-language-0.0.19.vsix`. The extension is distributed inside those archives, without a separate VSIX download asset. This website does not provide an in-browser interpreter.

After extraction, choose **… → Install from VSIX…** in the VS Code Extensions view, open `ku-language-0.0.19.vsix`, and reload the window. Alternatively, run this from the extracted directory:

```sh
code --install-extension ./ku-language-0.0.19.vsix
```

The extension also needs the Ku CLI. Add the extracted directory to `PATH`, or set `ku.executablePath` in VS Code to the full path of `ku` (`ku.exe` on Windows).

## Απαιτήσεις εκτέλεσης και κατασκευής

| Ενέργεια                                | Πρόσθετες απαιτήσεις                                                                                    |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Ku CLI για την αντίστοιχη πλατφόρμα                                                                     |
| Προεπιλεγμένο `ku build hello.ku`       | Rust **1.89.0** για τον στόχο και ο συνδέτης του συστήματος· διατηρήστε τις παρεχόμενες εξαρτήσεις Rust |
| `ku build --backend c hello.ku`         | Λειτουργικός μεταγλωττιστής C και συνδέτης για τον στόχο                                                |
| Εγγενές πρόγραμμα βάσης δεδομένων       | Αντίστοιχες εξωτερικές δυναμικές βιβλιοθήκες πελάτη βάσης δεδομένων· δεν περιλαμβάνονται                |

Ο προεπιλεγμένος εκκινητής ενσωματώνει μόνο την πηγή του σημείου εισόδου. Οι εισαγόμενες μονάδες εξακολουθούν να φορτώνονται από τις αρχικές θέσεις τους. Η ανάπτυξη πλήρους τοπικού γράφου εισαγωγών ανεξάρτητα από τις πηγές απαιτεί εγγενή μεταγλώττιση μέσω C και υπόκειται στα [όρια υποστήριξης backend](/el/docs/support/).

## Δοκιμάστε την πηγή ανάπτυξης

Από ένα τοπικό αντίγραφο του αποθετηρίου:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.19`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Απεγκατάσταση

Για εγκατάσταση από συμπιεσμένο αρχείο, αφαιρέστε τον κατάλογο αποσυμπίεσης, αναιρέστε κάθε προσθήκη στο `PATH` και απεγκαταστήστε την επέκταση VS Code. Πρώτα διατηρήστε όσα αρχεία πηγής και ρυθμίσεων δημιουργήσατε εκεί. Δεν έχει επαληθευτεί ξεχωριστός εγκαταστάτης με ένα κλικ, εγκατάσταση μέσω διαχειριστή πακέτων ή εντολή `ku uninstall`.

Επόμενο: [εκτελέστε το πρώτο σας πρόγραμμα](/el/docs/getting-started/). Πηγή της σύμβασης εγκατάστασης: [αρχεία εκδόσεων](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/el/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
