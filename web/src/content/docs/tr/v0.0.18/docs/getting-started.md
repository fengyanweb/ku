---
title: 'Başlangıç'
description: 'Hello Ku yazın, kaynak kodunuzu denetleyin ve yorumlama ile derlemeyi ayırt edin.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'examples/hello.ku',
    'README.md',
    'docs/syntax.md',
    'docs/v0.0.18.md',
    'docs/v0.0.18-r1.md',
  ]
slug: tr/v0.0.18/docs/getting-started
---

Önce [Ku kurulumunu](/tr/v0.0.18/docs/install/) yapın ve `ku --version` komutunun çalıştığını doğrulayın.

## İlk programınızı yazın

Depodaki `examples/hello.ku` ile aynı içeriğe sahip `hello.ku` dosyasını oluşturun:

```ku
fn main() {
    println("Hello Ku")
}
```

`fn main()` giriş noktasıdır. `println` bir satır metin yazar. Ku, `let` veya `let mut` kullanmaz.

## Denetleyin ve çalıştırın

Dosyanın bulunduğu dizinde:

```sh
ku check hello.ku
ku run hello.ku
```

`check`, programı çalıştırmadan sözcüksel, sözdizimsel ve temel anlamsal hataları denetler. Çalıştırıldığında beklenen standart çıktı:

```text
Hello Ku
```

Ku `PATH` içinde değilse yürütülebilir dosyayı tam kurulum yoluyla çağırın. Windows yürütülebilir dosyası `ku.exe` adındadır.

## Değişkenler ve işlevler

Bu temel sözdizimi parçası `main` içine yerleştirilebilir:

```ku
name = "Ku"
count:int = 3
println(name)
println(count)
```

İlk atama değişkeni bildirir. Açık tür belirtimi `name:type = value` biçimindedir. `print` otomatik satır sonu olmadan yazar; `println` satır sonu ekler.

## Yürütülebilir dosya oluşturun

Kurulum sayfası gerekli derleme araçlarını listeler. Varsayılan arka uç için:

```sh
ku build hello.ku
```

Varsayılan arka uç bir yorumlayıcı çalıştırıcısını paketler. C üzerinden yerel derleme ayrı bir C derleyicisi gerektirir:

```sh
ku build --backend c hello.ku
```

Varsayılan çıktı `.ku/build/<profile>/` altındadır; CLI tarafından bildirilen tam yolu kullanın. İki arka uç farklı özellikleri kapsar. `ku check` başarısı yerel derlemenin başarılı olacağını kanıtlamaz.

Not: `ku build --native hello.ku` komutu **`-o` olmadan yalnızca C kaynak kodu üretir**, bağlama yapmaz. İlk derlenmiş programınız için yukarıdaki `--backend c` komutunu kullanın.

## Proje oluşturun

Paket dizini gerektiğinde yerleşik bir şablon kullanın:

```sh
ku create HelloWorld --template basic
cd HelloWorld
ku check
ku run
```

Paketin giriş noktası `ku.mod` içinde yapılandırılır. `ku create --list` kullanılabilir şablonları listeler. Şu anda `ku test` veya `ku fmt` komutları yoktur.

[Dil kılavuzu](/tr/v0.0.18/docs/guide/language/), [sorun giderme](/tr/v0.0.18/docs/guide/errors/) ve [arka uç desteği](/tr/v0.0.18/docs/support/) ile devam edin.
