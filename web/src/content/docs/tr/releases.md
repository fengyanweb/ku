---
title: 'Sürüm notları'
description: 'Ku v0.0.19 experimental prerelease scope, downloads and known limitations.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/v0.0.19.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
  ]
---

## 0.0.19

Güncel belgeler, CLI ve VS Code uzantısıyla aynı sürüm numarasını kullanan deneysel Ku 0.0.19 ön sürümünü kapsar. Kullanılabilirlik, dosyalar ve sağlama toplamları için gerçek GitHub Release esas alınır.

[Release · SHA256SUMS](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [Installation](/tr/docs/install/) · [Release notes (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.en.md) · [Known issues (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.en.md)

## v0.0.18 · Geçmiş

Bu belgeler v0.0.18 deneysel ön sürümünün dondurulmuş kapsamını açıklar. Gerçek yayımlanma durumu ve indirmeler GitHub Releases üzerinde yer alır.

> Aşağıdaki v0.0.18 güncellemeleri şimdilik İngilizcedir; çevirinin geri kalanı korunmuştur. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

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

## v0.0.17 · Deneysel ön sürüm

**2026-10-18 (America/Los_Angeles)** tarihinde, UTC ile `2026-10-19T03:51:10Z` anında yayımlandı. Sürüm `prerelease=true` ve `draft=false` değerlerine sahiptir. Şu anda hiçbir kararlı sürüm doğrulanmamıştır.

Ana değişiklikler:

- Eşzamanlı salt okunur ödünç parametreler normal çağrı argümanlarıyla `&name: T` kullanır. Normal sahip olunan parametreler taşıma anlamını korur.
- Ku ile yazılan ayrıştırıcı alt kümesi, sınırlı iç içelik ve deyim sayısıyla süslü parantezli `while` ekler.
- Platformlar arası testler stdout bağlantı kesilmesi denetimlerini düzeltir. Bu bir iş uygulaması API'si eklemez.
- Arşivler varsayılan çalıştırıcıya uygun Rust bağımlılıklarını ve hedef TLS paketini içerir. Uygun dış araçlar yine gereklidir.

İndirmeler Windows x64, Linux x64 ve macOS arm64 içindir. Yedi sürüm dosyası üç platform arşivi, üç bildirim ve `SHA256SUMS` dosyasıdır. [Kuruluma](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) ve [tam sürüm notlarına](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) bakın.

### Uyumluluk ve sınırlar

Ku deneysel olmaya devam eder ve üretime hazır değildir. 0.0.x sürümü kaynak veya üretilen C/FFI ABI uyumluluğu vaat etmez. Güncellerken ilgili yerel çıktıları yeniden derleyin. Ödünç async parametreler, tam kendi kendini barındırma ve tam yerel async bu sürümün dışındadır.

Varsayılan çalıştırıcı yalnızca giriş kaynağını gömer; içe aktarılan modüller hâlâ özgün konumları gerektirir. Veritabanı istemcileri yalnızca yereldir. Her sürücünün gerçek sunucu, TLS ve oturum durumu sınırlarını denetleyin. Ku'nun kendi koduna hâlâ açık kaynak lisansı verilmemiştir.
