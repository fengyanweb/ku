---
title: 'Υποστήριξη πλατφορμών και backend'
description: 'Διακρίνετε τις πλατφόρμες Ku, τον διερμηνευτή, τον εκκινητή, το εγγενές C, το υποσύνολο Task και το LLVM.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
---

Η τεκμηρίωση περιγράφει το οριστικοποιημένο εύρος της πειραματικής προέκδοσης v0.0.18. Η πραγματική δημοσίευση και οι λήψεις καταγράφονται στα GitHub Releases.

## Δημοσιευμένες πλατφόρμες CLI

Η πειραματική προέκδοση v0.0.18 καλύπτει Windows x64 (MSVC), Linux x64 (GNU) και macOS arm64 (Apple Silicon). Κάθε πλατφόρμα έχει δικό της αρχείο. Δεν υπάρχει ένα εκτελέσιμο και για τα τρία συστήματα. Δεν επαληθεύτηκαν άλλα αρχεία πλατφορμών· δείτε την [εγκατάσταση](/el/docs/install/).

## Διαδρομές εκτέλεσης

| Δυνατότητα                                        | Διερμηνευτής `ku run`            | Προεπιλεγμένος εκκινητής `ku build`             | Σύγχρονο εγγενές C                                       | Εγγενές Task ανάπτυξης                           | Πρωτότυπο κειμένου LLVM  |
| ------------------------------------------------- | -------------------------------- | ----------------------------------------------- | -------------------------------------------------------- | ------------------------------------------------ | ------------------------ |
| Βασική γλώσσα, συναρτήσεις, ροή ελέγχου           | Υλοποιημένα                      | Διαδρομή διερμηνευτή                            | Υλοποιημένο υποσύνολο                                    | Περιορισμένο τυποποιημένο υποσύνολο              | Περιορισμένο πρωτότυπο   |
| JSON                                              | Υποστηρίζεται                    | Υποστηρίζεται                                   | Υποστηρίζεται                                            | Δεν υποστηρίζεται                                | Δεν υποστηρίζεται        |
| Πλήρης τοπικός γράφος εισαγωγών ανεξάρτητος πηγών | Φορτώνει πηγές                   | Δεν υποστηρίζεται· ενσωματώνεται μόνο η είσοδος | Επεκτείνεται κατά την παραγωγή                           | Επεκτείνεται στο υποστηριζόμενο υποσύνολο        | Χωρίς υπόσχεση ανάπτυξης |
| async / await                                     | Υλοποιημένο με όρια πόρων        | Διαδρομή διερμηνευτή                            | Ειδικό υποσύνολο ή απόρριψη                              | Μερικώς, όχι πλήρες async                        | Χωρίς πλήρη υποστήριξη   |
| Πελάτες PostgreSQL / Redis / MySQL                | Χωρίς πρόσβαση βάσης             | Χωρίς διερμηνευόμενη πρόσβαση βάσης             | Μόνο εγγενώς                                             | Όχι γενική αναμονή Task                          | Δεν υποστηρίζονται       |
| Πελάτης TCP / TLS                                 | Χωρίς αντίστοιχη εγγενή διαδρομή | Χωρίς αντίστοιχη εγγενή διαδρομή                | Παραγόμενη υλοποίηση C· το TLS απαιτεί αντίστοιχο πακέτο | Χωρίς ισχυρισμό πλήρους γενικής αναμονής δικτύου | Δεν υποστηρίζεται        |

Οι προεπιλεγμένες κατασκευές απαιτούν τα αντίστοιχα εργαλεία Rust και συνδέτη. Το εγγενές C απαιτεί εργαλεία C στόχου. Η επιτυχία του `ku check` δεν αποδεικνύει ότι λειτουργεί καμία κατασκευή. Μια εντολή LLVM δεν συνεπάγεται πλήρη backend έκδοσης.

## Το εγγενές υποσύνολο Task

Ο τρέχων κλάδος ανάπτυξης έχει περιορισμένη διαδρομή πηγής `async fn main(): null!`. Καλύπτει ρητά απαριθμημένους πρωτογενείς τύπους, συμβολοσειρές, επίπεδους πίνακες, Results ενός επιπέδου, εκκίνηση/μετακίνηση/await Task, ορισμένες ροές ελέγχου και περιορισμένες λειτουργίες αρχείων/καταλόγων στο παρασκήνιο. Δεν είναι πλήρης ασύγχρονος backend στο δημοσιευμένο πακέτο v0.0.18.

Η γενική αναμονή δικτύου Task, η πλήρης εκτέλεση M:N, οι αυθαίρετοι συνδυασμοί δυναμικού σωρού, όλοι οι συνδυασμοί δεσμεύσεων/συναρτήσεων async και οι πλήρεις δοκιμές μακράς διάρκειας παραμένουν ανολοκλήρωτα. Δείτε το ακριβές αποδεκτό και απορριπτόμενο υποσύνολο στη [σύμβαση συντρέχουσας εκτέλεσης](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md).

## Όρια βάσεων δεδομένων και δικτύου

Τα API βάσεων χρησιμοποιούν `module.client(config)?` και στη συνέχεια μεθόδους του δέκτη, με περιορισμένη δεξαμενή μέσα στον πελάτη. Η επαλήθευση πραγματικού διακομιστή γίνεται ξεχωριστά για κάθε οδηγό, σύστημα και commit. Ένα προσομοιωμένο ABI ή επιτυχής σύνδεση δεν αντικαθιστά πραγματικό ερώτημα.

Το Redis χρησιμοποιεί απλό, μη κρυπτογραφημένο TCP. Το MySQL δεν διαθέτει συνεπή επαλήθευση πιστοποιητικών με υποχρεωτική απόρριψη σε αποτυχία σε Oracle/MariaDB. Οι απευθείας συνδέσεις πρέπει επομένως να χρησιμοποιούν loopback, ελεγχόμενα ιδιωτικά δίκτυα ή επαληθευμένη σήραγγα TLS. Το απομακρυσμένο PostgreSQL απαιτεί ρητό `sslmode=verify-full` και έμπιστα πιστοποιητικά ρίζας. Το εγγενές TLS στο `std.net` δεν σημαίνει ότι HTTP, Redis ή ο διερμηνευτής χρησιμοποιούν την ίδια διαδρομή.

## Αυτοφιλοξενία και συμβατότητα

Ο λεξικός αναλυτής και ο parser γραμμένοι στο Ku έχουν περιορισμένα υποσύνολα και διαφορικούς ελέγχους. Πλήρης αυτοφιλοξενούμενος parser, ελεγκτής, IR και backend παραμένουν ανολοκλήρωτα· το Rust εξακολουθεί να παρέχει τα εργαλεία αρχικής εκκίνησης. Η έκδοση 0.0.x δεν υπόσχεται συμβατότητα πηγής ή παραγόμενου C/FFI ABI.

Τα ημερολόγια εργασίας και προηγούμενα αποτελέσματα CI τεκμηριώνουν μόνο το δηλωμένο εύρος και commit. Χρησιμοποιήστε τις [σημειώσεις εκδόσεων](/el/releases/) για να ξεχωρίσετε δημοσιευμένες αλλαγές από ανάπτυξη. Ο ιστότοπος δεν αναφέρει ως επιτυχημένα όλα τα τρέχοντα CI του κλάδου, παραγωγικά φορτία ή αποδοχή βάσεων δεδομένων.

> Οι νέες πληροφορίες για τη v0.0.18 ακολουθούν προς το παρόν στα αγγλικά· η υπόλοιπη μετάφραση διατηρείται. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions.

### Process memory boundary

| Host    | `ku supervise` guarantee and prerequisite                                                                                                                                                                                                                                                                   |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment.                                                          |
| macOS   | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge.                                                                                                           |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.
