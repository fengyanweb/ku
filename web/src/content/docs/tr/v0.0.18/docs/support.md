---
title: 'Platform ve arka uç desteği'
description: 'Ku sürüm platformlarını, yorumlayıcıyı, çalıştırıcıyı, yerel C, Task alt kümesi ve LLVM desteğini ayırt edin.'
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
slug: tr/v0.0.18/docs/support
---

## Yayımlanmış CLI platformları

v0.0.18 deneysel ön sürümü Windows x64 (MSVC), Linux x64 (GNU) ve macOS arm64 (Apple Silicon) platformlarını kapsar. Her platformun ayrı arşivi vardır. Üç sistem için tek bir yürütülebilir dosya yoktur. Diğer platform dosyaları doğrulanmadı; [kuruluma](/tr/v0.0.18/docs/install/) bakın.

## Yürütme yolları

| Yetenek                                          | Yorumlayıcı `ku run`          | Varsayılan `ku build` çalıştırıcısı  | Eşzamanlı yerel C                              | Yerel Task (alt küme)              | LLVM metin prototipi |
| ------------------------------------------------ | ----------------------------- | ------------------------------------ | ---------------------------------------------- | ---------------------------------- | -------------------- |
| Temel dil, işlevler, kontrol akışı               | Uygulandı                     | Yorumlayıcı yolu                     | Uygulanmış alt küme                            | Sınırlı türlenmiş alt küme         | Sınırlı prototip     |
| JSON                                             | Desteklenir                   | Desteklenir                          | Desteklenir                                    | Desteklenmez                       | Desteklenmez         |
| Kaynaktan bağımsız tam yerel içe aktarma grafiği | Kaynak yükler                 | Desteklenmez; yalnızca giriş gömülür | Üretim sırasında açılır                        | Desteklenen alt kümede açılır      | Dağıtım sözü yok     |
| async / await                                    | Kaynak sınırlarıyla uygulandı | Yorumlayıcı yolu                     | Ayrı alt küme veya ret                         | Kısmi, tam async değil             | Tam destek yok       |
| PostgreSQL / Redis / MySQL istemcileri           | Veritabanı erişimi yok        | Yorumlanan veritabanı erişimi yok    | Yalnızca yerel                                 | Genel Task beklemesi değil         | Desteklenmez         |
| TCP / TLS istemcisi                              | Karşılık gelen yerel yol yok  | Karşılık gelen yerel yol yok         | Üretilen C uygulaması; TLS eşleşen paket ister | Tam genel ağ beklemesi iddiası yok | Desteklenmez         |

Varsayılan derlemeler eşleşen Rust araçlarını ve bağlayıcıyı gerektirir. Yerel C hedef C araçlarını gerektirir. `ku check` başarısı iki derlemeden birinin çalıştığını kanıtlamaz. LLVM komutunun bulunması tam bir sürüm arka ucu anlamına gelmez.

## Yerel Task alt kümesi

Sürüm sınırlı yerel C yolu `async fn main(): null!` içerir: listelenen ilkeller, dizgiler, düz diziler, tek katmanlı Results, Task başlatma/taşıma/await, bazı kontrol akışları ve sınırlı dosya/dizin arka plan işlemleri. Yerel TCP/TLS sabit blocking pool kullanır. Bu genel ağ readiness veya tam M:N değildir; kesin kapsam için sürümün eşzamanlılık başvurusuna bakın.

Genel Task ağ beklemesi, tam M:N yürütmesi, rastgele dinamik yığın birleşimleri, tüm async yakalama/işlev birleşimleri ve tam uzun süreli dayanıklılık testleri tamamlanmamıştır. Kabul ve ret edilen kesin alt küme için [eşzamanlılık sözleşmesine](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) bakın.

## Veritabanı ve ağ sınırları

Veritabanı API'leri `module.client(config)?` ve ardından alıcı yöntemlerini kullanır; istemci içinde sınırlı havuz bulunur. Gerçek sunucu doğrulaması her sürücü, sistem ve commit için ayrıdır. Sahte ABI veya başarılı bağlama gerçek sorgunun yerini tutmaz.

Redis şifrelenmemiş TCP kullanır. MySQL'de Oracle/MariaDB genelinde hata durumunda kesin ret sağlayan tutarlı sertifika doğrulaması yoktur. Doğrudan bağlantılar bu nedenle loopback, denetimli özel ağ veya doğrulanmış TLS tüneli kullanmalıdır. Uzak PostgreSQL açık `sslmode=verify-full` ve güvenilir kök sertifikalar gerektirir. `std.net` içindeki yerel TLS, HTTP, Redis veya yorumlayıcının aynı yolu kullandığı anlamına gelmez.

## Kendi kendini barındırma ve uyumluluk

Ku ile yazılmış sözcüksel çözümleyici ve ayrıştırıcı sınırlı alt kümelere ve diferansiyel denetimlere sahiptir. Tam bir öz ayrıştırıcı, denetleyici, IR ve arka uç tamamlanmamıştır; başlangıç araçlarını hâlâ Rust sağlar. 0.0.x sürümü kaynak veya üretilen C/FFI ABI uyumluluğu vaat etmez.

## v0.0.18 kapsamı ve sınırları

- JSON: `encode`/`decode`, `stringify`/`parse` ile aynıdır ve `1e20` gidiş-dönüş düzeltmesini içerir.
- Yerel Task listelenen sınırlı alt kümedir; kapanış kabul ve temizlik düzeltmeleri tam M:N veya genel ağ askısı değildir.
- HTTP sınırlı socket readiness, kuyruklar, yanıtlar ve keep-alive ile isteğe bağlı sınırlı tanı sunar; tüm yük kabulünün geçtiği anlamına gelmez.
- `ku supervise` bütün program worker'ları, kabul, kuyruk baytları, çıktı ve temizliği yönetir; istek veya Task başına worker değildir ve normal `ku run` için otomatik uygulanmaz.

**Bilinen HTTP sorunu:** aralıklı HTTP 504 hataları oluşabilir. Nedeni henüz doğrulanmamıştır.

### İşlem belleği sınırları

| İşlem belleği sınırları | `ku supervise`                                                                                                                                                                                                                                        |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux                   | cgroup v2 çekirdek muhasebesindeki bellek; yazılabilir, devredilmiş domain kökü ve `memory`, `pids`, `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max`, `cgroup.kill` gerekir. Eksik kontrol veya izin uygulamayı başlatmadan reddeder. |
| Windows                 | İç içe Job Objects worker ve grubu RSS değil, tahsis taahhüdü verilen sanal bellekle sınırlar. Askıdaki uygulama iş parçacığı bırakılmadan ilk commit denetlenir; ana makine politikası Job atamasını reddedebilir.                                   |
| macOS                   | Bu yalıtım yerel desteklenmez; uygulama başlamadan reddedilir. Gerektiğinde yapılandırılmış Linux makine/VM kullanın; otomatik VM yönlendirmesi yoktur.                                                                                               |

Worker kaynak alanı Ku ve dış kütüphane tahsislerini sayar. Linux cgroup ve Windows commit farklı ölçülerdir. `memory.max` geçici aşımı kabul edebilir; tüm fiziksel belleğin her an mutlak tavanı garanti edilmez.

Koordinatörün OS sert bellek sınırı yoktur; rezerv ve kabul muhasebesi kesin RSS değildir. Bir Task'ın tüm heap'i tam ölçülmez; havuz, önbellek ve kütüphaneler Task başına kesin ayrılamaz. Dış veritabanı, makine/VM, derleyici ve normal `ku run` otomatik worker sınırında değildir.

OS worker'ı sonlandırırsa koordinatör hata bildirir ve rezervi yalnızca temizliği doğruladıktan sonra döndürür. Bazı tahsisler önceden reddedilebilir, fakat dış kütüphane tükenmesi her zaman normal Result olmaz. **Veritabanı işlemleri otomatik yinelenmez**; worker çıkışı rollback kanıtlamaz, yan etkiler bilinmeyebilir.

[`ku supervise`](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/process-execution.md) · [Task](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md) · [HTTP 504](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/http-stability-acceptance.md)
