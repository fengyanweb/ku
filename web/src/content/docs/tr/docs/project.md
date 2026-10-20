---
title: 'Proje ve katkı'
description: 'Ku projesinin durumu, herkese açık kaynak, lisans sınırları ve yeniden üretilebilir sorun raporları.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

Bu belgeler v0.0.18 deneysel ön sürümünün dondurulmuş kapsamını açıklar. Gerçek yayımlanma durumu ve indirmeler GitHub Releases üzerinde yer alır. Proje **Ku programming language / Ku 编程语言** adını taşır; CLI adı `ku` olarak belirlenmiştir. Ku geliştirilmekte olan küçük bir programlama dili ve yorumlayıcıdır. Güncel herkese açık sürümü v0.0.18 deneysel ön sürümüdür.

## Kaynak ve lisanslama

[GitHub kaynağını](https://github.com/fengyanweb/ku) ve [sürüm sayfasını](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) inceleyin. Kaynak herkese açık görüntülenebilir, ancak Ku'nun kendi koduna **açık kaynak lisansı verilmemiştir**. Düzenleyici uzantısı `UNLICENSED` olarak işaretlidir. Kaynak veya ikili dosya yayımlamak otomatik olarak açık kaynak lisansı vermez.

Sürüm arşivlerindeki üçüncü taraf bildirimleri yalnızca ilgili bileşenlere uygulanır. Ku'nun kendisini kullanma, değiştirme veya yeniden dağıtma izni oluşturmaz. Gerekli yetki proje sorumlusundan açıkça gelmelidir; bu site yeni lisans taahhütleri oluşturmaz.

## Sorun bildirin

Yeniden üretilebilir raporlar için deponun [Issues](https://github.com/fengyanweb/ku/issues) bölümünü kullanın. Şunları ekleyin:

- Ku sürümü, geliştirme kaynakları için Git commiti ve hedef sistem / mimari.
- Tam komut, arka uç ve en küçük `.ku` kaynak örneği.
- Tam tanı veya yapılandırılmış Error, beklenen ve gerçek çıktı.
- Derleme sorunları için Rust / C araç sürümleri ve yerel içe aktarmaların bulunup bulunmadığı.

Token, parola, gerçek bağlantı dizgisi veya hassas iş verisi içeren günlükler göndermeyin. Performans raporları ortamı, iş yükünü, ölçüm yöntemini ve ham sonuçları içermelidir. Tek başarılı örnek üretim kapasitesini kanıtlamaz.

## Katkıdan önce

[Sözdizimi sözleşmesi](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) ve [anlam sözleşmesi](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) dil davranışını tanımlar. CI, uygulama ve sözleşmeleri birlikte inceleyin. Çalışma günlüğündeki hedef uygulanmış özellik değildir.

Değişiklikleri odaklı tutun; yeniden üretimi, nedeni, kapsamı ve doğrulamayı açıklayın. Site içeriğini ilgili kaynakla birlikte gözden geçirin. Mevcut testleri, çalışma ağacı değişikliklerini ve benzersiz kanıtları koruyun. Katkı kabulünü ve lisans düzenlemelerini proje sorumlusu belirler; site katkı sözleşmesi uydurmaz.

Bu site statiktir. Metni, gezinmesi ve kodu iş uygulaması arka ucu gerektirmez. Arama tarayıcıya bir Pagefind indeksi yükler; site uzaktan Ku kodu yürütme sağlamaz.

[Başlangıç](/tr/docs/getting-started/), [destek sınırları](/tr/docs/support/) ve [sürüm notları](/tr/releases/) ile devam edin.
