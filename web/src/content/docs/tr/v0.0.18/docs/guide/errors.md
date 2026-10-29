---
title: 'Hatalar ve sorun giderme'
description: 'Ku kurulumu, içe aktarma, sahiplik, JSON ve arka uç yetenek hatalarını tanılayın.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/diagnostics.md',
    'docs/syntax.md',
    'src/error.rs',
    'src/stdlib/json.rs',
    'docs/v0.0.18-r1.md',
  ]
slug: tr/v0.0.18/docs/guide/errors
---

Derleyici tanıları deneysel geliştirme sırasında değişebilir. Sorun bildirirken CLI sürümünü, Git commitini, arka ucu, tam komutu ve en küçük kaynak örneğini ekleyin.

## Ku yürütülebilir dosyasını belirleyin

```sh
ku --version
ku -h
ku check hello.ku
```

`ku` bulunamazsa yürütülebilir dosyayı çıkarıldığı dizinden çağırın. Windows üzerinde `ku.exe` kullanın. Varsayılan derlemede Rust veya bağlama kütüphaneleri eksikse [kurulum gereksinimlerini](/tr/v0.0.18/docs/install/) denetleyin ve yalnızca CLI'yi kopyalamak yerine tam arşiv düzenini koruyun.

## Tanıları okuyun

Normal denetleme dosyayı, kaynak konumunu, kaynak parçasını ve mevcut düzeltme önerilerini bildirir. Araç bütünleşmeleri şunu kullanabilir:

```sh
ku check --json hello.ku
```

Çıktı **JSON Lines** biçimindedir; her satırda bir tanı bulunur. Alanlar arasında `level`, `code`, `message`, `file`, `line`, `column`, `endLine`, `endColumn`, `notes` ve `helps` vardır. Başarı sessizdir; JSON çıktısı olmaması denetimin çalışmadığı anlamına gelmez.

| Tanı    | Yaygın neden                          | Yapılacak işlem                                                                       |
| ------- | ------------------------------------- | ------------------------------------------------------------------------------------- |
| `E0105` | `let` kullanımı                       | Değişkeni ilk atamayla bildirin                                                       |
| `E0302` | bool olmayan `if` / `while` koşulu    | Açık bir mantıksal karşılaştırma yazın                                                |
| `E0601` | Bilinmeyen standart kütüphane modülü  | `std.json` gibi gerçek modül yolunu denetleyin                                        |
| `E0605` | Gerekli açık içe aktarmanın eksikliği | Modülü içe aktarın; örneğin `import fs from "std.fs"`                                 |
| `E0901` | Taşınmış değeri okuma                 | Bağımsız bir değeri açıkça klonlayın veya desteklenen eşzamanlı ödünç almayı kullanın |
| `E0913` | Ödünç async parametre                 | Sahip olunan parametre kullanın; asıl değer gerekiyorsa çağıran tarafta klonlayın     |
| `E0918` | Çağrı yerinde `&value` yazılması      | Normal argüman geçirin; ödünç almayı imza bildirir                                    |

Bu, kayıtlı tanıların özetidir; her hatanın listesi değildir. Güncel belirleyici kayıt [src/error.rs](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/src/error.rs) içindedir.

## Çalışma zamanı hatalarının ayrı bir sözleşmesi vardır

Çalışma zamanı Error `domain/code` değerleri derleyicinin `E` kodlarından ayrıdır. Geçersiz JSON, `json/parse_error`; olmayan nesne anahtarı `object/missing_key` döndürebilir. Başarısız olabilecek çağrıdan sonra `?` kullanın veya alanlarını `try/catch` içinde okuyun.

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

Bu öğretici program bilerek kurtarılabilir ayrıştırma hatası üretir. Bildirilen `byte N`, sıfır tabanlı UTF-8 bayt konumudur. Çince metin veya emoji çevresindeki karakter sayısı değildir.

## Yerel derleme reddedildi

`ku check` başarısı yalnızca ön uç denetiminin başarılı olduğunu gösterir. Yerel C ve LLVM'nin kendi yetenek sınırları vardır. Bir yapı açıkça reddedilirse [destek matrisini](/tr/v0.0.18/docs/support/) denetleyin. Zaman aşımını artırmak veya başka arka uçta başarılı olmak desteklenmeyen yolu doğrulamaz.

`ku build --native file.ku`, `-o` olmadan yalnızca C yazar. Bir yürütülebilir dosya derleyip bağlamak için `ku build --backend c file.ku` kullanın ve hedef C araçlarını kurun.

Veritabanı hataları `execution_unknown` ve `execution_completed_without_result` otomatik yeniden yürütmeyi tetiklememelidir. İstek ve iş işlemine ait kanıtları koruyun ve önce gerçek yürütme durumunu belirleyin.

Ayrıntılar için [depodaki tanı kılavuzuna](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/diagnostics.md) bakın.

JSON hatalarının domain/code alanları arka uçlarda aynıdır, fakat message farklı olabilir. ku run ve runner json parse failed: önekini ekleyebilir, yerel C eklemeyebilir. Tam metin yerine domain/code karşılaştırın.
