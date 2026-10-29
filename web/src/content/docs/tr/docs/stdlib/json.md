---
title: 'JSON başvurusu'
description: 'Ku JSON kodlama, çözme, KuValue dönüşümü, hata kodları ve arka uç desteği.'
kuVersion: '0.0.19'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
---

`std.json`, JSON kodlar ve çözer. `json.encode` girdiyi salt okunur ödünç alır; `json.decode` bağımsız sahiplikli `KuValue` oluşturur. İkisi de Result döndürür. `stringify`, `parse`, `try_parse` uyumlu adlar olarak kalır.

## İçe aktarma ve arayüzler

```ku
import json from "std.json"
```

| Arayüz                       | Dönüş      | Anlam                                                |
| ---------------------------- | ---------- | ---------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`     | Yeni, sıkıştırılmış JSON metnine salt okunur kodlama |
| `json.parse(text:str)`       | `KuValue!` | Bağımsız sahiplikli dinamik değere katı ayrıştırma   |
| `json.try_parse(text:str)`   | `KuValue!` | `parse` için uyumluluk takma adı                     |
| `json.encode(&value:any)`    | `str!`     | Bu sürümde bulunan `stringify` takma adı             |
| `json.decode(text:str)`      | `KuValue!` | Bu sürümde bulunan `parse` takma adı                 |

`!`, Result anlamına gelir. Hataları iletmek için `?`, ele almak için `try/catch` kullanın. Kodlama girdiyi tüketmez veya değiştirmez.

## Değerleri kodlama ve okuma

Tam temel örnek; diğer kullanımlar: [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.19/examples/json.ku).

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.encode(value)?
    println(text)

    decoded = json.decode(text)?
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

`1.0`, `1` olarak kodlanabilir. Float'ın tamsayı metni int64 dışındaysa kodlayıcı `.0` ekler: `1e20`, `100000000000000000000.0` olur ve yeniden çözülür. Sonlu binary64 özgün yazımı, float etiketini veya negatif sıfırın işaretini korumaz. Düzeltme bu sürümdedir.

## Hatalar

Kurtarılabilir hatalar `json` etki alanını kullanır. Yaygın kodlar `parse_error`, `stringify_error` ve `out_of_memory` değerleridir. Ayrıştırma iletisindeki `byte N`, sıfır tabanlı UTF-8 bayt konumudur. En küçük geçersiz girdi işleyicisi için [hatalar ve sorun gidermeye](/tr/docs/guide/errors/) bakın.

## Arka uç desteği

Yorumlayıcı, varsayılan paketlenmiş çalıştırıcı ve eşzamanlı yerel C arka ucu JSON uygulamalarına sahiptir. **Sınırlı yerel Task alt kümesi ve LLVM, JSON desteklemez.** Varsayılan çalıştırıcının içe aktarmaları kaynak konumlarına bağımlıdır; JSON desteği tamamen bağımsız paket anlamına gelmez.

[std.json](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md#127-json).
