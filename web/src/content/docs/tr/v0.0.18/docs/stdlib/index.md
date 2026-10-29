---
title: 'Standart kütüphane'
description: 'Ku standart kütüphane modüllerini, temel imzaları ve arka uç sınırlarını bulun.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
    'docs/v0.0.18-r1.md',
  ]
slug: tr/v0.0.18/docs/stdlib
---

[Sözdizimi başvurusu](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) standart kütüphaneyi açıklar. Arka uç sınırları için [destek matrisine](/tr/v0.0.18/docs/support/) bakın.

## Modülleri içe aktarma

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`, `http`, `config`, `task`, `pg`, `redis`, `mysql`, `bytes` ve `net` açık içe aktarma gerektirir. Bazı eski modüller hâlâ doğrudan çağrılabilir; yeni örnekler bağımlılıkları görünür kılmak için açık içe aktarmayı tercih eder. `std.task`, uygulama eşzamanlılık API'si değil, iç gözlem ve stres testi ad alanıdır.

| Modül                                | Amaç                                              | Dikkat edilecek sınırlar                                                        |
| ------------------------------------ | ------------------------------------------------- | ------------------------------------------------------------------------------- |
| `std.string` / dizgi yöntemleri      | Unicode uzunluğu, bayt uzunluğu, arama, dilimleme | Yerel `trim/lower/upper` için Unicode uygulaması yoktur                         |
| `std.array` / dizi yöntemleri        | Uzunluk, arama, yeni diziler, map                 | Doğrudan indeks hatası kurtarılabilir `try_get` hatasından farklıdır            |
| `std.object`                         | `get_or` ile açık yedek değerli arama             | Olmayan sıradan indeks varsayılan olarak null olmaz                             |
| [std.json](/tr/v0.0.18/docs/stdlib/json/)    | Katı JSON kodlama ve çözme                        | Yerel Task ve LLVM içinde desteklenmez                                          |
| `std.fs`                             | Dosyalar, varlık denetimleri, dizin listeleme     | Göreli yollar geçerli `.ku` kaynak dizininden çözülür                           |
| `std.time`                           | Zaman, tarihler, süreler, uyku                    | Eşzamanlı ve Task beklemeleri farklı yürütme yolları kullanır                   |
| `std.http`                           | HTTP istemcileri, hizmetler, yanıtlar             | Arka uç ve yaşam döngüsü farkları sürer; tam genel Task ağı uygulaması değildir |
| `std.config`                         | Ortam ve yapılandırma                             | Gerçek kimlik bilgilerini kaynak koda kaydetmeyin                               |
| `std.pg` / `std.redis` / `std.mysql` | Sınırlı havuzlu birleşik istemciler               | Yalnızca yerel; gerçek sunucu ve TLS sınırları ayrıca doğrulanmalıdır           |
| `std.bytes` / `std.net`              | İkili veri, TCP istemcisi, isteğe bağlı TLS       | Üretilen yerel C yolu; TLS eşleşen hedef paketini gerektirir                    |
| `std.lexer` / `std.parser`           | Derleyici hata ayıklama yardımcıları              | Tam açık Token / AST yapıları yerine hata ayıklama özetleri döndürür            |

## Yaygın imzalar

Bunlar belge imzalarıdır. `&` desteklenen salt okunur parametre kipini, `T!` ise kurtarılabilir sonucu gösterir:

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

`"中😀".len()` 2, `.byte_len()` 7 değerindedir. Uzunluk Unicode skaler değerlerini sayar; dilimleme UTF-8 baytları yerine karakter indekslerini kullanır. `array.push` ve `array.concat` girdilerini değiştirmeden yeni diziler döndürür.

## Dosya okuma

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

Önce kaynak dosyanın yanında UTF-8 `message.txt` oluşturun. Olmayan dosya veya başarısız okuma hata döndürür. Bu örnek o girdi dosyasına bağımlıdır. `try_read` ve `try_write` aynı sonuç türlerine sahip uyumlu takma adlar olarak kalır.

`read_dir` sıralanmış ve bağımsız sahiplikli yollar döndürür; sınırlar 10.000 girdi ve toplam 1.000.000 yol baytıdır. Sınırlı yerel Task dosya işlemleri sabit bir arka plan havuzu kullanır. Bu durum aynı mekanizmayla genel ağ beklemesini kanıtlamaz.
