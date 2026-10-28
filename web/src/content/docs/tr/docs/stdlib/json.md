---
title: 'JSON başvurusu'
description: 'Ku JSON kodlama, çözme, KuValue dönüşümü, hata kodları ve arka uç desteği.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.19.md',
  ]
---

> Aşağıdaki v0.0.18 güncellemeleri şimdilik İngilizcedir; çevirinin geri kalanı korunmuştur. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## İçe aktarma ve arayüzler

```ku
import json from "std.json"
```

| Arayüz                       | Dönüş      | Anlam                                                |
| ---------------------------- | ---------- | ---------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Yeni, sıkıştırılmış JSON metnine salt okunur kodlama |
| `json.parse(text:str)`       | `KuValue!` | Bağımsız sahiplikli dinamik değere katı ayrıştırma   |
| `json.try_parse(text:str)`   | `KuValue!` | `parse` için uyumluluk takma adı                     |
| `json.encode(&value:any)`    | `str!`     | Geliştirme dalında `stringify` takma adı             |
| `json.decode(text:str)`      | `KuValue!` | Geliştirme dalında `parse` takma adı                 |

`!`, Result anlamına gelir. Hataları iletmek için `?`, ele almak için `try/catch` kullanın. Kodlama girdiyi tüketmez veya değiştirmez.

## Değerleri kodlama ve okuma

Bu tam bir temel örnektir. Geliştirme dalında [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.19/examples/json.ku) içinde daha kapsamlı bir regresyon örneği vardır.

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

Nesne anahtarları belirlenimci biçimde sıralanır. Beklenen çıktı:

```text
{"count":7,"name":"Ku"}
Ku
8
```

Ayrıştırılmış `KuValue` doğrudan tamsayı aritmetiğine katılamaz. Önce `.as_int()?` veya `.as_str()?` ile dönüştürün. Uyumsuz tür hata döndürür. Olmayan nesne anahtarları da otomatik varsayılan yerine hata döndürür.

## Veri ve sınırlar

Desteklenen girdiler dizgileri, mantıksal değerleri, tamsayıları, sonlu kayan noktalı sayıları, null, iç içe dizileri, dizgi anahtarlı nesneleri ve temsil edilebilir yapıları kapsar. İşlevler, Tasks, kaynak tanıtıcıları, Results, enumlar ve döngüsel yapılar kodlanamaz.

| Kural                       | Güncel uygulama                                                                                    |
| --------------------------- | -------------------------------------------------------------------------------------------------- |
| JSON sözdizimi              | Katı; yorumları, sondaki virgülleri, değer sonrası fazladan girdiyi ve geçersiz Unicode'u reddeder |
| Girdi / çıktı               | Her biri 1.000.000 baytla sınırlıdır                                                               |
| Derinlik bütçesi            | Kök derinliği 0; `depth > 32` reddedilir; 33 boş kapsayıcı düzeyi kabul edilir, 34 reddedilir      |
| Tamsayılar                  | İşaretli 64 bit; taşma hatadır, otomatik float dönüşümü yoktur                                     |
| Kayan nokta                 | Sonlu binary64; özgün yazım, float etiketleri ve negatif sıfır korunmaz                            |
| Unicode                     | Çince, emoji, kaçışlar, gömülü NUL                                                                 |
| Yinelenen nesne anahtarları | Ayrıştırma son değeri korur                                                                        |
| Anahtar sırası              | Kodlama UTF-8 bayt sırasına göre sıralar                                                           |

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## Hatalar

Kurtarılabilir hatalar `json` etki alanını kullanır. Yaygın kodlar `parse_error`, `stringify_error` ve `out_of_memory` değerleridir. Ayrıştırma iletisindeki `byte N`, sıfır tabanlı UTF-8 bayt konumudur. En küçük geçersiz girdi işleyicisi için [hatalar ve sorun gidermeye](/tr/docs/guide/errors/) bakın.

## Arka uç desteği

Yorumlayıcı, varsayılan paketlenmiş çalıştırıcı ve eşzamanlı yerel C arka ucu JSON uygulamalarına sahiptir. **Sınırlı yerel Task alt kümesi ve LLVM, JSON desteklemez.** Varsayılan çalıştırıcının içe aktarmaları kaynak konumlarına bağımlıdır; JSON desteği tamamen bağımsız paket anlamına gelmez.

Bu sayfadaki imzalar ve sınırlar kaynak kod ve sözleşmelerle karşılaştırıldı. Yürütme doğrulaması sürüm, commit ve arka uç belirtmelidir. Tam [JSON sözdizimi sözleşmesine](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md#127-json) bakın.
