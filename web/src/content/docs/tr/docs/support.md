---
title: 'Platform ve arka uç desteği'
description: 'Ku sürüm platformlarını, yorumlayıcıyı, çalıştırıcıyı, yerel C, Task alt kümesi ve LLVM desteğini ayırt edin.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Ku 0.0.19 indirme

Ku 0.0.19 deneysel bir ön sürümdür. macOS ARM64 (Apple Silicon), Windows x64 ve Linux x64 (GNU) paketleri yayımlanmıştır.

[Paketler ve kurulum adımları](/tr/docs/install/)

## Çalıştırma biçimleri

- **`ku run`**: Dil özellikleri, genel JSON ve senkron HTTP işleyicileri sunan yorumlayıcıdır; veritabanı istemcileri yoktur.
- **`ku build`**: Varsayılan runner giriş dosyasını içerir; yerel importlar hâlâ kaynak dosyalara ihtiyaç duyar. Rust **1.89.0** ve sistem bağlayıcısı gerekir.
- **`ku build --backend c`**: Senkron JSON, `http.json` içeren HTTP ve yerel PostgreSQL, Redis ve MySQL istemcilerini destekler. Hedef C derleyicisi, bağlayıcısı ve uyumlu istemci kitaplıkları gerekir. Veritabanı işlemleri otomatik yeniden denenmez.
- **Task**: Yerel Task yolu; başlatma/`await`, zamanlayıcılar, seçili I/O işlemleri, adlandırılmış async HTTP işleyicileri ve belirli PG sorguları içeren türlenmiş bir alt kümeyi destekler. Bu yolda JSON yalnızca skaler değerli nesne değişmezlerini kodlayabilir; genel çözümleme yoktur. Tam M:N ve genel I/O kapsamı henüz tamamlanmamıştır.
- **LLVM**: LLVM metin arka ucu sınırlı bir prototip olarak kalmaktadır.

## Bilinen sorunlar

Üç platformun sürüm ve paket kontrolleri başarıyla tamamlandı. Bu, üretim kullanımı için genel bir onay değildir.

- **HTTP 504:** geçmişte görüldü ancak son kısa kontrolde yeniden üretilemedi; nedeni doğrulanmadı.
- **İptal sırasında gecikme sıçramaları:** son 30 saniyelik kontrolde 60.000 kısa isteğin tamamı doğruydu. p99 **1.557 ms**, en yüksek değer **98.809 ms** idi; bazı istekler hâlâ 20 ms’yi aştı.

Resmî 30 dakikalık kararlılık kabulü henüz geçilmedi. Kısa ölçümler bunun yerini tutmaz.

Tam başvuru kaynağı: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

Sorun bildirin: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
