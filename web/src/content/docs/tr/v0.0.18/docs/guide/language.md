---
title: 'Dil kılavuzu'
description: 'Ku değişkenlerini, işlevlerini, kontrol akışını, sahipliği, modülleri ve kurtarılabilir hataları öğrenin.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/syntax.md',
    'docs/semantics.md',
    'examples/function.ku',
    'examples/control_flow.ku',
    'examples/result.ku',
    'docs/v0.0.18-r1.md',
  ]
slug: tr/v0.0.18/docs/guide/language
---

Bu kılavuz temel sözdizimini tanıtır. Deponun [sözdizimi başvurusu](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/syntax.md) ve [anlam başvurusu](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/semantics.md) belirleyici olmaya devam eder. Bunlar öğretici parçalardır; farklar için [arka uç desteğine](/tr/v0.0.18/docs/support/) bakın.

## Değişkenler ve türler

```ku
name = "Ku"
count:int = 3
ready:bool = true
values:[int] = [1, 2, 3]
count += 1
```

İlk atama değişkeni bildirir. İlkel türler `int`, `float`, `bool`, `str` ve `null` türleridir. Tamsayılar işaretli 64 bittir; kayan noktalı sayılar binary64 kullanır. Tamsayılar örtük doğruluk dönüşümüyle koşula dönüşmez.

## İşlevler ve kontrol akışı

Bu tam temel örnek `examples/function.ku` dosyasından alınmıştır:

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    println(add(10, 20))
}
```

Beklenen çıktı: `30`. Bu kontrol akışı parçası `main` içine yerleştirilebilir:

```ku
total = 0
for i in 5 {
    total += i
}
if (total == 10) {
    println("sum ok")
}
```

`for i in 5`, `0` ile `4` arasında yineler. `if` ve `while`, `bool` koşullar ister. Birden fazla deyim için süslü parantez kullanın. Dallanma ifadeleri `match` kullanır; `switch` yoktur.

<a id="ownership"></a>

## Sahiplik ve salt okunur ödünç alma

Owned değerler varsayılan taşınır ve sahip kapsamı bittiğinde belirlenimci drop uygulanır. Bağımsız kopya `.clone()` gerektirir. Ku genel izlemeli GC kullanmaz; closure ortamları gibi paylaşılan iç yapılar referans sayımı ve çalışma zamanı maliyeti kullanabilir. Eşzamanlı salt okunur ödünç alma `&name: T` imzasıyla bildirilir. Normal Owned parametre yalnızca okusa bile değeri taşıyabilir. Çağrı `length(value)` kalır, `&value` kullanılmaz; async ödünç alma desteklenmez.

`int`, `float`, `bool` ve `null`, Copy değerleridir. Dizgiler, diziler, nesneler, yapılar, enumlar, Results ve işlev değerleri Owned türündedir. Atama ve sıradan argümanlar sahip olunan değerleri varsayılan olarak taşır. Bağımsız bir değer gerektiğinde `.clone()` kullanın. Tasks klonlanamaz.

```ku
original = "Ku"
copy = original.clone()
println(original)
println(copy)
```

Eşzamanlı bir işlev salt okunur ödünç parametre bildirebilir:

```ku
fn length(&text: str): int {
    return text.len()
}
```

Argümanda `&` olmadan `length(value)` olarak çağırın. Ödünç veriler değiştirilemez, içlerinden değer taşınamaz veya çağrı dışına kaçırılamaz. Ödünç async parametreler gibi birleşimler henüz desteklenmez. Salt okunur ödünç alma, işlevin her bakımdan saf olacağını garanti etmez.

## Yapılar, nesneler ve modüller

```ku
struct User {
    name: str
    age: int
}

user = User { name: "Ku", age: 1 }
println(user.name)
```

Dinamik nesneler `{ name: "Ku" }` kullanır. Olmayan bir nesne indeksine sıradan erişim hata verir. `value["name"]?`, aramayı Result üzerinden ele alır ve başarıda `KuValue` verir. Somut tür için `.as_str()?` veya `.as_int()?` kullanın. Olmayan anahtar sessizce `null` değerine dönüşmez.

Standart kütüphane içe aktarmaları `import json from "std.json"` gibi noktalı yollar kullanır. Yerel modüller `import math from "./math"` kullanabilir. Kullanıcı tanımlı üst düzey dışa aktarma adları ASCII büyük harfle başlar; küçük harfli adlar dosyaya özel kalır.

## Kurtarılabilir hatalar

`T!` dönüş türü T veya Error döndürebilir. `?` kurtarılabilir hatayı iletir; `fail` oluşturur:

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    try {
        println(read_name()?)
    } catch (error) {
        println(error.message)
    }
}
```

Error, `domain`, `code` ve `message` alanlarına sahiptir. Sıradan `catch`, `panic`, Task iptali veya zaman aşımını yakalamaz. İptal sırasında finally temizliği mutlak son süre bütçesini paylaşır; kullanıcıya ait her türlü temizliğin biteceğini garanti etmez.

## Eşzamanlılık sınırları

Uygulama kodu bir `async fn` çağırarak tek kullanımlık Task tanıtıcısı alır ve ardından `await` kullanır. `task.spawn`, `Task.new` veya `thread.spawn` adlı uygulama API'leri yoktur. Yorumlayıcı ve yerel Task alt kümesinin kapsamı farklıdır. Bir arka uçta çalışan örnek diğerinde tam destek olduğunu kanıtlamaz. [Platform ve arka uç desteğine](/tr/v0.0.18/docs/support/) bakın.
