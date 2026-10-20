---
title: Kurulum ve indirme
description: Ku 0.0.18 deneysel ön sürümünü indirin; platform ve derleme
  gereksinimlerini öğrenin.
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - Cargo.toml
  - docs/v0.0.18.md
  - docs/history.md
  - .github/scripts/export-release.py
slug: tr/v0.0.18/docs/install
---

> Aşağıdaki v0.0.18 güncellemeleri şimdilik İngilizcedir; çevirinin geri kalanı korunmuştur. [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

This page targets the **v0.0.18 experimental prerelease**. Use the actual [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) and its assets; documentation alone does not establish completed publication or production readiness.

## Platformunuzu seçin

Download the target archive below, its manifest and `SHA256SUMS` from the [v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). `native_ci-*` artifacts are CI examples, not CLI installers.

| Sistem  | Mimari                | Arşiv                                        |
| ------- | --------------------- | -------------------------------------------- |
| macOS   | Apple Silicon / arm64 | `ku-v0.0.18-aarch64-apple-darwin.tar.gz`     |
| Windows | x64 / MSVC            | `ku-v0.0.18-x86_64-pc-windows-msvc.tar.gz`   |
| Linux   | x64 / GNU             | `ku-v0.0.18-x86_64-unknown-linux-gnu.tar.gz` |

Diğer platform arşivleri doğrulanmadı. Çapraz derleme desteği her işletim sistemi veya mimari için yayımlanmış paket olduğu anlamına gelmez.

## Çıkarın ve doğrulayın

1. Arşivi sürümdeki `SHA256SUMS` ile doğrulayın. Ayrı bir `.manifest.json`, arşiv ve dosya bütünlüğünü açıklar. Sağlama toplamları bütünlük denetimidir; yayıncı imzası değildir.
2. **Arşivin tamamını** boş bir dizine çıkarın ve `README-INSTALL.md` dosyasını okuyun. `ku`, `libku.rlib`, `deps/` ve `native-tls/` arasındaki göreli dizilimi koruyun.
3. O dizinden sürümü denetleyin. macOS / Linux üzerinde:

```sh
./ku --version
```

Windows PowerShell içinde:

```powershell
.\ku.exe --version
```

Bildirilen sürüm `0.0.18` olmalıdır. İsterseniz bu dizini `PATH` içine ekleyip `ku --version` çalıştırın. VS Code bütünleşmesi için paketteki `ku-language-0.0.18.vsix` dosyasını kurun.

## Çalıştırma ve derleme gereksinimleri

| İşlem                                   | Ek gereksinimler                                                                            |
| --------------------------------------- | ------------------------------------------------------------------------------------------- |
| `ku run hello.ku` / `ku check hello.ku` | Eşleşen platform için Ku CLI                                                                |
| Varsayılan `ku build hello.ku`          | Hedef için Rust **1.89.0** ve sistem bağlayıcısı; paketlenmiş Rust bağımlılıklarını koruyun |
| `ku build --backend c hello.ku`         | Çalışan bir hedef C derleyicisi ve bağlayıcısı                                              |
| Yerel veritabanı programı               | Eşleşen harici veritabanı istemcisi dinamik kütüphaneleri; pakete dahil değildir            |

Varsayılan çalıştırıcı yalnızca giriş kaynağını gömer. İçe aktarılan modüller özgün konumlarından yüklenmeye devam eder. Tam bir yerel içe aktarma grafiğinin kaynaklardan bağımsız dağıtımı, C üzerinden yerel derleme gerektirir ve [arka uç sınırlarına](/tr/v0.0.18/docs/support/) tabidir.

## Geliştirme kaynak kodunu deneyin

Deponun yerel bir kopyasında:

```sh
cargo +1.89.0 run --locked --release -- --version
cargo +1.89.0 run --locked --release -- run examples/hello.ku
```

The CLI and Cargo version for this release is `0.0.18`. Record the Git commit, target and toolchain when building source; source-build success does not prove publication of a release artifact.

## Kaldırma

Arşiv kurulumu için çıkardığınız dizini silin, eklediğiniz `PATH` girdisini geri alın ve VS Code uzantısını kaldırın. Önce orada oluşturduğunuz kaynak dosyalarını ve yapılandırmayı koruyun. Ayrı bir tek tık kurucusu, paket yöneticisi kurulumu veya `ku uninstall` komutu doğrulanmadı.

Sıradaki adım: [ilk programınızı çalıştırın](/tr/v0.0.18/docs/getting-started/). Kurulum sözleşmesinin kaynağı: [sürüm arşivleri](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/history.md).

`ku supervise` has platform-specific prerequisites and memory metrics; see [support boundaries](/tr/v0.0.18/docs/support/). The macOS CLI can run programs, but this isolation mode is not natively supported.
