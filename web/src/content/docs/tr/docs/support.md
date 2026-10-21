---
title: 'Platform ve arka uç desteği'
description: 'Ku sürüm platformlarını, yorumlayıcıyı, çalıştırıcıyı, yerel C, Task alt kümesi ve LLVM desteğini ayırt edin.'
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

Bu belgeler v0.0.18 deneysel ön sürümünün dondurulmuş kapsamını açıklar. Gerçek yayımlanma durumu ve indirmeler GitHub Releases üzerinde yer alır.

## Yayımlanmış CLI platformları

v0.0.18 deneysel ön sürümü Windows x64 (MSVC), Linux x64 (GNU) ve macOS arm64 (Apple Silicon) platformlarını kapsar. Her platformun ayrı arşivi vardır. Üç sistem için tek bir yürütülebilir dosya yoktur. Diğer platform dosyaları doğrulanmadı; [kuruluma](/tr/docs/install/) bakın.

## Yürütme yolları

| Yetenek                                          | Yorumlayıcı `ku run`          | Varsayılan `ku build` çalıştırıcısı  | Eşzamanlı yerel C                              | Geliştirmedeki yerel Task          | LLVM metin prototipi |
| ------------------------------------------------ | ----------------------------- | ------------------------------------ | ---------------------------------------------- | ---------------------------------- | -------------------- |
| Temel dil, işlevler, kontrol akışı               | Uygulandı                     | Yorumlayıcı yolu                     | Uygulanmış alt küme                            | Sınırlı türlenmiş alt küme         | Sınırlı prototip     |
| JSON                                             | Desteklenir                   | Desteklenir                          | Desteklenir                                    | Desteklenmez                       | Desteklenmez         |
| Kaynaktan bağımsız tam yerel içe aktarma grafiği | Kaynak yükler                 | Desteklenmez; yalnızca giriş gömülür | Üretim sırasında açılır                        | Desteklenen alt kümede açılır      | Dağıtım sözü yok     |
| async / await                                    | Kaynak sınırlarıyla uygulandı | Yorumlayıcı yolu                     | Ayrı alt küme veya ret                         | Kısmi, tam async değil             | Tam destek yok       |
| PostgreSQL / Redis / MySQL istemcileri           | Veritabanı erişimi yok        | Yorumlanan veritabanı erişimi yok    | Yalnızca yerel                                 | Genel Task beklemesi değil         | Desteklenmez         |
| TCP / TLS istemcisi                              | Karşılık gelen yerel yol yok  | Karşılık gelen yerel yol yok         | Üretilen C uygulaması; TLS eşleşen paket ister | Tam genel ağ beklemesi iddiası yok | Desteklenmez         |

Varsayılan derlemeler eşleşen Rust araçlarını ve bağlayıcıyı gerektirir. Yerel C hedef C araçlarını gerektirir. `ku check` başarısı iki derlemeden birinin çalıştığını kanıtlamaz. LLVM komutunun bulunması tam bir sürüm arka ucu anlamına gelmez.

## Yerel Task alt kümesi

Güncel geliştirme dalında sınırlı bir `async fn main(): null!` kaynak yolu vardır. Açıkça listelenen ilkelleri, dizgileri, düz dizileri, tek katmanlı Results, Task başlatma/taşıma/await işlemlerini, bazı kontrol akışlarını ve sınırlı dosya/dizin arka plan işlemlerini kapsar. Yayımlanan v0.0.18 paketinde tam bir async arka uç değildir.

Genel Task ağ beklemesi, tam M:N yürütmesi, rastgele dinamik yığın birleşimleri, tüm async yakalama/işlev birleşimleri ve tam uzun süreli dayanıklılık testleri tamamlanmamıştır. Kabul ve ret edilen kesin alt küme için [eşzamanlılık sözleşmesine](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) bakın.

## Veritabanı ve ağ sınırları

Veritabanı API'leri `module.client(config)?` ve ardından alıcı yöntemlerini kullanır; istemci içinde sınırlı havuz bulunur. Gerçek sunucu doğrulaması her sürücü, sistem ve commit için ayrıdır. Sahte ABI veya başarılı bağlama gerçek sorgunun yerini tutmaz.

Redis şifrelenmemiş TCP kullanır. MySQL'de Oracle/MariaDB genelinde hata durumunda kesin ret sağlayan tutarlı sertifika doğrulaması yoktur. Doğrudan bağlantılar bu nedenle loopback, denetimli özel ağ veya doğrulanmış TLS tüneli kullanmalıdır. Uzak PostgreSQL açık `sslmode=verify-full` ve güvenilir kök sertifikalar gerektirir. `std.net` içindeki yerel TLS, HTTP, Redis veya yorumlayıcının aynı yolu kullandığı anlamına gelmez.

## Kendi kendini barındırma ve uyumluluk

Ku ile yazılmış sözcüksel çözümleyici ve ayrıştırıcı sınırlı alt kümelere ve diferansiyel denetimlere sahiptir. Tam bir öz ayrıştırıcı, denetleyici, IR ve arka uç tamamlanmamıştır; başlangıç araçlarını hâlâ Rust sağlar. 0.0.x sürümü kaynak veya üretilen C/FFI ABI uyumluluğu vaat etmez.

Çalışma günlükleri ve önceki CI sonuçları yalnızca belirtilen kapsam ve commiti doğrular. Yayımlanan değişiklikleri geliştirmeden ayırmak için [sürüm notlarını](/tr/releases/) kullanın. Bu site güncel dalın tüm CI denetimlerinin, üretim yüklerinin veya veritabanı kabulünün geçtiğini bildirmez.

> Aşağıdaki v0.0.18 güncellemeleri şimdilik İngilizcedir; çevirinin geri kalanı korunmuştur. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

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
