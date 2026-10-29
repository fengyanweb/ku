---
title: 'Sürüm notları'
description: 'Ku 0.0.18 — Sürüm notları'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
---

## 0.0.19

Güncel belgeler, CLI ve VS Code uzantısıyla aynı sürüm numarasını kullanan deneysel Ku 0.0.19 ön sürümünü kapsar. Kullanılabilirlik, dosyalar ve sağlama toplamları için gerçek GitHub Release esas alınır.

[Release · SHA256SUMS](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) · [Installation](/tr/docs/install/) · [Release notes (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19.en.md) · [Known issues (English)](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/v0.0.19-known-issues.en.md)

## v0.0.18

Çağrı zincirlerinde alıcının yinelenen değerlendirilmesi, yerel C float sabitleri ve `time.date(Time): Date!` düzeltilir, VS Code uzantısı güncellenir.

- JSON: `encode`/`decode`, `stringify`/`parse` ile aynıdır ve `1e20` gidiş-dönüş düzeltmesini içerir.

[Ku 0.0.18 indirin](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Platform ve arka uç desteği](/tr/v0.0.18/docs/support/)

## v0.0.17 · Deneysel ön sürüm

Ana değişiklikler:

- Eşzamanlı salt okunur ödünç parametreler normal çağrı argümanlarıyla `&name: T` kullanır. Normal sahip olunan parametreler taşıma anlamını korur.
- Ku ile yazılan ayrıştırıcı alt kümesi, sınırlı iç içelik ve deyim sayısıyla süslü parantezli `while` ekler.
- Arşivler varsayılan çalıştırıcıya uygun Rust bağımlılıklarını ve hedef TLS paketini içerir. Uygun dış araçlar yine gereklidir.

İndirmeler Windows x64, Linux x64 ve macOS arm64 içindir. Yedi sürüm dosyası üç platform arşivi, üç bildirim ve `SHA256SUMS` dosyasıdır. [Kuruluma](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) ve [tam sürüm notlarına](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) bakın.

### Uyumluluk ve sınırlar

Ku deneysel olmaya devam eder ve üretime hazır değildir. 0.0.x sürümü kaynak veya üretilen C/FFI ABI uyumluluğu vaat etmez. Güncellerken ilgili yerel çıktıları yeniden derleyin. Ödünç async parametreler, tam kendi kendini barındırma ve tam yerel async bu sürümün dışındadır.

Varsayılan çalıştırıcı yalnızca giriş kaynağını gömer; içe aktarılan modüller hâlâ özgün konumları gerektirir. Veritabanı istemcileri yalnızca yereldir. Her sürücünün gerçek sunucu, TLS ve oturum durumu sınırlarını denetleyin. Ku'nun kendi koduna hâlâ açık kaynak lisansı verilmemiştir.
