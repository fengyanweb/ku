---
title: 'Kurulum ve indirme'
description: 'Ku 0.0.19 deneysel ön sürümünü indirin; platform ve derleme gereksinimlerini öğrenin.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Bilgisayarınız için **Ku 0.0.19** paketini indirin. Bu deneysel ön sürümün her paketi komut satırı aracını, yorumlayıcıyı ve VS Code eklentisini içerir.

## 1. Ku’yu indirin

| Sistem  | Mimari                | İndir                                                                                                                          | Boyut   |
| ------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------------ | ------- |
| macOS   | Apple Silicon / ARM64 | [İndir macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)     | 39.8 MB |
| Windows | x64                   | [İndir Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz) | 44.1 MB |
| Linux   | x64 / GNU             | [İndir Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) | 48.3 MB |

## 2. Çıkarın ve çalıştırın

**Paketin tamamını** boş bir klasöre, örneğin kullanıcı klasörünüzdeki `ku` klasörüne çıkarın. Tüm dosyaları bir arada tutun; yalnızca çalıştırılabilir dosyayı taşımayın. Bu klasörde bir terminal açın.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

Çıktıda `ku 0.0.19` görünmelidir. Artık `.ku` dosyalarını çalıştırıp denetleyebilirsiniz; bu işlemler için Rust kurulumu gerekmez.

<details>
<summary>İsteğe bağlı: ku’yu her klasörden kullanın</summary>

Paketi `~/ku` dizinine çıkardıysanız kabuğunuzun başlangıç dosyasına, örneğin `~/.zshrc` veya `~/.bashrc` dosyasına şu satırı ekleyin:

```sh
export PATH="$HOME/ku:$PATH"
```

Windows’ta «Hesabınız için ortam değişkenlerini düzenleyin» seçeneğini açın, kullanıcı `Path` değişkenini düzenleyin ve çıkardığınız klasörün tam yolunu, örneğin `%USERPROFILE%\ku`, ekleyin. Mevcut girdileri koruyun.

Yeni bir terminal açıp `ku --version` komutunu çalıştırın.

</details>

## 3. VS Code eklentisini kurun

VS Code’un Eklentiler görünümünde **… → VSIX’ten Yükle…** seçeneğini kullanın, çıkardığınız klasördeki `ku-language-0.0.19.vsix` dosyasını seçin ve pencereyi yeniden yükleyin.

Ku `PATH` içinde değilse `ku.executablePath` ayarını `ku` dosyasının (Windows’ta `ku.exe`) tam yoluna ayarlayın.

<details>
<summary>İndirmeyi doğrulayın</summary>

Paketin SHA-256 değerini [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS) ile karşılaştırın. [Sürüm sayfasında](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) her paket için dosya sağlama toplamlarını içeren bir `.manifest.json` da bulunur. Ek talimatlar paketteki `README-INSTALL.md` dosyasındadır.

</details>

<details>
<summary>Kendi Ku programlarınızı derleyin</summary>

`ku run` ve `ku check` için yalnızca platformun CLI aracı gerekir. Varsayılan `ku build` ayrıca Rust **1.89.0** ve sistem bağlayıcısı gerektirir. `ku build --backend c` hedef platform için C derleyicisi ve bağlayıcı gerektirir. Yerel veritabanı programları ayrıca uygun veritabanı istemci kitaplıklarına ihtiyaç duyar; bunlar pakete dahil değildir.

</details>

<details>
<summary>Kaldırma</summary>

Oluşturduğunuz kaynak dosyaları ve yapılandırmaları koruyun; ardından çıkardığınız klasörü, eklediğiniz `PATH` girdisini ve VS Code eklentisini kaldırın.

</details>

[Sonraki adım: ilk programınızı çalıştırın](/tr/docs/getting-started/)
