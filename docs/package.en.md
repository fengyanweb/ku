# Installing and sharing Ku packages

`ku i` and `ku package install` use the same installer. The commands in this
guide use a Ku installation built from a checkout that contains the installer.
Check `ku help package install` to confirm your version supports it.

## Install a public Git package

You need Git, Python 3, the Rust toolchain named in the repository's
`rust-toolchain.toml`, and a C compiler for native applications.

```sh
git clone https://github.com/fengyanweb/ku.git ku-source
cd ku-source
python3 -B scripts/install-runtime-from-source.py --prefix ../ku-sdk
cd ..
mkdir consumer
cd consumer
../ku-sdk/ku init
../ku-sdk/ku i 'https://github.com/fengyanweb/ku.git#780e3955473eaaf1d7b101ed68c946c7173866c9:examples/package'
```

In PowerShell, invoke the installer with `python` and the executable with
`..\ku-sdk\ku.exe`. Keep the Git URL quoted on either shell.

Copy the printed import into `src/main.ku`:

```ku
import { Value } from "@demo_pkg/util"

fn main() {
    println(Value())
}
```

```sh
../ku-sdk/ku build --release --locked -o app
./app
```

The program prints `7`. Use `-o app.exe` and `./app.exe` on Windows. Add
`--backend runner` to build the explicit Runner instead. Both outputs for this
example run without a Ku installation or the package author's checkout.
`ku run --locked` interprets the project directly.
If a Windows GNU build produces `app.exe.windows-gnu-notices.txt`,
keep it alongside the executable when distributing the app.

## Restore and share

- `ku i ./vendor/library` adds a portable project-relative dependency. Put the
  package's `ku.mod` and its source directory there.
- `ku i 'https://host/owner/repo.git#v1.2.3'` installs a Git package. The
  installer records the full commit. A full commit can be followed by
  `:path/to/package` if the manifest lives below the repository root.
- `ku i tools` resolves a name from the project's explicitly configured
  signed Ku registry. Ku does not host a public name directory. See the
  [registry protocol](registry-api.md) for setting up an HTTPS registry and
  pinning its Ed25519 public key.
- `ku i` restores the existing pins and prepares missing artifacts. Run
  `ku package resolve` when you intentionally want to refresh versions.
- `ku i --locked --prod` prepares the production scope without changing
  the lock. `ku i --offline` uses only verified local material. Choose
  `--locked` or `--offline`. Use `--dev` with a source to add a development
  dependency.

To share your own package, use `ku init --template lib`, give it a unique
`name` and version in `ku.mod`, and commit the manifest and exported `.ku`
modules to your Git repository. Exported names start with an uppercase ASCII
letter. Give consumers the HTTPS repository URL and the commit or tag to
install. With a registry and an authorized name, use `ku package pack` and
`ku package publish` as described in the registry protocol.

## Foreign libraries

For automatic C discovery, put one supported header and one C implementation
in `vendor/your_sdk` and run `ku i ./vendor/your_sdk`. Ku generates an
editable package under `ku-libraries`. Commit the package and use its
`ku.mod` to share it. Use a source description for interfaces whose ownership
cannot be inferred. The [SQLite and TinyXML-2 examples](../examples/interop-native)
show an annotated C adapter and a C++ owner with a checked lifetime.

When packages must share one native instance, put the foreign adapter in its
own Ku package and have both consumers depend on that package. Ku resolves one
provider version and prepares one bridge for that application. Independently
bundled static copies have separate state. If two paths to a provider require
different sources or incompatible versions, installation reports the conflict.

The signed Ku registry can carry a source-only adapter with
`interop.version = "1"`. This pins its descriptor and allowed native or Python
sources with the same signed archive checksum used for pure Ku packages.
Compiled bridges are prepared on the consumer's platform. A registry upload
cannot include prebuilt shared libraries or install scripts.

For Python, `ku i ./tools/subtitles.py --module text` can generate a module
from statically visible functions. To install PyPI artifacts use, for example,
`ku i pypi:Pillow --module PIL.Image`; the distribution and Python module
are separate. See [the Python guide](interop-python.md) for interpreter
selection, dynamic arguments and delivery.

Foreign providers are loaded only when called. Deliver the executable with
its adjacent `ku-runtime.d` directory whenever the build produces one.
Third-party binary ABI requirements apply even when the executable is moved.
Keep each provider's license and notices with its material. Foreign calls
currently belong in synchronous application paths.
