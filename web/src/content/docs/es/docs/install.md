---
title: 'Instalación y descargas'
description: 'Descarga Ku 0.0.20 para tu equipo. Cada paquete incluye la herramienta de línea de comandos, el intérprete y la extensión de VS Code.'
kuVersion: '0.0.20'
verifiedAt: '2026-11-09'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.20.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

Descarga **Ku 0.0.20** para tu equipo. Cada paquete incluye la herramienta de línea de comandos, el intérprete y la extensión de VS Code.

## 1. Descargar Ku

| Sistema | Arquitectura          | Descargar                                                                                                                          |
| ------- | --------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / ARM64 | [Descargar macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-aarch64-apple-darwin.tar.gz)     |
| Windows | x64                   | [Descargar Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-pc-windows-msvc.tar.gz) |
| Linux   | x64 / GNU             | [Descargar Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-unknown-linux-gnu.tar.gz) |

## 2. Extraer y ejecutar

Extrae el **paquete completo** en una carpeta vacía, por ejemplo `ku` dentro de tu carpeta de usuario. Conserva todos los archivos juntos; no muevas solo el ejecutable. Abre una terminal en esa carpeta.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

Debe aparecer `ku 0.0.20`. Ya puedes ejecutar y comprobar archivos `.ku`; estas operaciones no requieren instalar Rust.

<details>
<summary>Opcional: usar ku desde cualquier carpeta</summary>

Si extrajiste el paquete en `~/ku`, añade esta línea al archivo de inicio de tu shell, como `~/.zshrc` o `~/.bashrc`:

```sh
export PATH="$HOME/ku:$PATH"
```

En Windows, abre «Editar las variables de entorno de esta cuenta», edita el `Path` del usuario y añade la ruta completa de la carpeta extraída, por ejemplo `%USERPROFILE%\ku`. Conserva las entradas existentes.

Abre una terminal nueva y ejecuta `ku --version`.

</details>

## 3. Instalar la extensión de VS Code

En la vista Extensiones de VS Code, selecciona **… → Instalar desde VSIX…**, abre `ku-language-0.0.20.vsix` en la carpeta extraída y recarga la ventana.

Si Ku no está en `PATH`, establece `ku.executablePath` en la ruta completa de `ku` (`ku.exe` en Windows).

<details>
<summary>Comprobar la descarga</summary>

Compara el SHA-256 del paquete con [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.20/SHA256SUMS). La [página de la versión](https://github.com/fengyanweb/ku/releases/tag/v0.0.20) también ofrece un `.manifest.json` por paquete con las sumas de los archivos. Consulta `README-INSTALL.md` dentro del paquete para más información.

</details>

<details>
<summary>Compilar tus propios programas Ku</summary>

`ku run` y `ku check` no requieren Rust. `ku build` necesita un enlazador del sistema y la identidad exacta del compilador Rust registrada por el SDK instalado; los paquetes v0.0.20 usan **Rust 1.99.1**. `ku build --backend c` necesita un compilador C y un enlazador para el destino. Los programas nativos de base de datos también necesitan bibliotecas cliente compatibles, que no se incluyen.

</details>

<details>
<summary>Desinstalar</summary>

Guarda los archivos fuente y la configuración que hayas creado. Después, elimina la carpeta extraída, la entrada de `PATH` que añadiste y la extensión de VS Code.

</details>

[Siguiente: ejecutar tu primer programa](/es/docs/getting-started/)
