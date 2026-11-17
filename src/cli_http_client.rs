//! Target-pinned, private native HTTP client archive. Unused programs never
//! inspect the optional pack or inherit its dependencies.
use super::{
    native_compression as manifest, pinned_file_bytes, pinned_file_sha256,
    require_native_tls_pack_file, BuildTarget, NativeBinaryFormat, StagedLinkLibrary,
};
use crate::{
    error::KuError,
    native_tls_archive::{self, NativeTlsArchiveFormat},
    span::Span,
    value::Value,
};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, env, fs, path::PathBuf};

const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
const BUILD_ID: &str = "ku-native-http-client/0.1.0;abi=1;ureq=2.12.1;url=2.5.8;engine=shared";
pub(super) struct StagedHttpClientInputs {
    pub(super) archive: StagedLinkLibrary,
    pub(super) libraries: Vec<String>,
    pub(super) msvc: bool,
}
fn error(message: impl Into<String>) -> KuError {
    KuError::message(message)
}
fn source_hash() -> String {
    let files: [(&str, &[u8]); 5] = [
        (
            "crates/ku-native-http-client/Cargo.toml",
            include_bytes!("../crates/ku-native-http-client/Cargo.toml"),
        ),
        (
            "crates/ku-native-http-client/build.rs",
            include_bytes!("../crates/ku-native-http-client/build.rs"),
        ),
        (
            "crates/ku-native-http-client/include/ku_native_http_client.h",
            include_bytes!("../crates/ku-native-http-client/include/ku_native_http_client.h"),
        ),
        (
            "crates/ku-native-http-client/src/lib.rs",
            include_bytes!("../crates/ku-native-http-client/src/lib.rs"),
        ),
        (
            "src/stdlib/http_client_engine.rs",
            include_bytes!("stdlib/http_client_engine.rs"),
        ),
    ];
    let mut hash = Sha256::new();
    for (name, bytes) in files {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(bytes);
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}
fn int(fields: &HashMap<String, Value>, key: &str, expected: i64) -> Result<(), KuError> {
    if !matches!(fields.get(key),Some(Value::Int(n)) if *n==expected) {
        return Err(error(format!(
            "native HTTP client manifest '{key}' must be {expected}"
        )));
    }
    Ok(())
}
fn require(fields: &HashMap<String, Value>, key: &str, expected: &str) -> Result<(), KuError> {
    if manifest::text(fields, key)? != expected {
        return Err(error(format!(
            "native HTTP client manifest '{key}' does not match this CLI/target"
        )));
    }
    Ok(())
}
pub(super) fn resolve(
    target: &BuildTarget,
    static_link: bool,
) -> Result<StagedHttpClientInputs, KuError> {
    let format = match (target.rust_triple, target.binary_format) {
        ("aarch64-apple-darwin", NativeBinaryFormat::MachOArm64) => {
            NativeTlsArchiveFormat::MachOArm64
        }
        ("x86_64-unknown-linux-gnu", NativeBinaryFormat::ElfX86_64) => {
            NativeTlsArchiveFormat::ElfX86_64
        }
        ("aarch64-unknown-linux-gnu", NativeBinaryFormat::ElfArm64) => {
            NativeTlsArchiveFormat::ElfArm64
        }
        ("x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu", NativeBinaryFormat::PeX86_64) => {
            NativeTlsArchiveFormat::CoffX86_64
        }
        _ => {
            return Err(error(
                "native HTTP client has no verified archive contract for this target",
            ))
        }
    };
    if static_link && target.rust_triple == "x86_64-pc-windows-msvc" {
        return Err(error("native HTTP client uses the MSVC dynamic CRT (/MD); omit --static or provide a separately verified compatible target"));
    }
    let root = if let Some(value) = env::var_os("KU_NATIVE_HTTP_CLIENT_PACK") {
        let root = PathBuf::from(value);
        if !root.is_absolute() {
            return Err(error("KU_NATIVE_HTTP_CLIENT_PACK must name an absolute pack directory; no fallback is performed"));
        }
        root
    } else {
        let executable =
            env::current_exe().map_err(|e| error(format!("cannot locate http_client SDK: {e}")))?;
        super::native_pack_root_beside(&executable, "native-http")?.join(target.rust_triple)
    };
    let root=fs::canonicalize(&root).map_err(|e|error(format!("native HTTP client target pack '{}' is unavailable: {e}; install the matching Ku SDK or set KU_NATIVE_HTTP_CLIENT_PACK",root.display())))?;
    if !super::path_is_plain_directory(&root) {
        return Err(error("native HTTP client pack must be a plain directory"));
    }
    let declaration =
        require_native_tls_pack_file(&root.join("manifest.json"), "native HTTP client manifest")?;
    let bytes = pinned_file_bytes(&declaration, 64 * 1024, "native HTTP client manifest")?;
    manifest::reject_duplicate_keys(&bytes)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| error("native HTTP client manifest must be UTF-8"))?;
    let decoded =
        crate::stdlib::json::eval("decode", &[Value::String(text.to_owned())], Span::default())?
            .ok_or_else(|| error("native HTTP client manifest decoder returned no value"))?;
    let Value::Result { ok: true, value } = decoded else {
        return Err(error("native HTTP client manifest must be strict JSON"));
    };
    let fields = manifest::object(&value, "http_client")?;
    manifest::keys(
        fields,
        &[
            "schema_version",
            "abi_version",
            "build_id",
            "crate_version",
            "ureq_version",
            "rustc_version",
            "rustc_verbose",
            "target",
            "source_sha256",
            "archive",
            "header",
            "native_static_libs",
            "licenses",
        ],
        &["build"],
    )?;
    int(fields, "schema_version", 1)?;
    int(fields, "abi_version", 1)?;
    require(fields, "build_id", BUILD_ID)?;
    require(fields, "crate_version", "0.1.0")?;
    require(fields, "ureq_version", "2.12.1")?;
    require(fields, "target", target.rust_triple)?;
    require(fields, "source_sha256", &source_hash())?;
    require(fields, "rustc_version", env!("KU_BUILD_RUSTC_RELEASE"))?;
    let verbose = manifest::text(fields, "rustc_verbose")?;
    if !verbose
        .lines()
        .any(|line| line == concat!("release: ", env!("KU_BUILD_RUSTC_RELEASE")))
        || !verbose
            .lines()
            .any(|line| line == concat!("commit-hash: ", env!("KU_BUILD_RUSTC_COMMIT")))
    {
        return Err(error(
            "native HTTP client compiler identity does not match the frozen toolchain",
        ));
    }
    // The archive is C ABI, not an rlib. Its own rustc must nevertheless match
    // the frozen release toolchain; no arbitrary SDK/CRT substitution is used.
    let archive_name = if target.rust_triple.ends_with("msvc") {
        "ku_native_http_client.lib"
    } else {
        "libku_native_http_client.a"
    };
    let declared_file = |value: &Value, name: &str, subdir: &str, limit: u64| {
        let decl = manifest::object(value, name)?;
        manifest::keys(decl, &["name", "size", "sha256"], &[])?;
        require(decl, "name", name)?;
        let size = match decl.get("size") {
            Some(Value::Int(n)) if *n > 0 && (*n as u64) <= limit => *n as u64,
            _ => return Err(error("invalid http_client file size")),
        };
        let path = root.join(subdir).join(name);
        let pinned = require_native_tls_pack_file(&path, "native HTTP client file")?;
        if pinned.identity.length != size
            || pinned_file_sha256(&pinned, limit, "native HTTP client file")?
                != manifest::sha256(manifest::text(decl, "sha256")?)?
        {
            return Err(error(format!(
                "native HTTP client file '{}' does not match manifest",
                path.display()
            )));
        }
        Ok(pinned)
    };
    let file = |key: &str, name: &str, subdir: &str, limit: u64| {
        declared_file(
            fields
                .get(key)
                .ok_or_else(|| error("missing http_client file declaration"))?,
            name,
            subdir,
            limit,
        )
    };
    let archive = file("archive", archive_name, "lib", MAX_ARCHIVE)?;
    let header = file("header", "ku_native_http_client.h", "include", 64 * 1024)?;
    if pinned_file_bytes(&header, 64 * 1024, "native HTTP client header")?
        != include_bytes!("../crates/ku-native-http-client/include/ku_native_http_client.h")
    {
        return Err(error(
            "native HTTP client header does not match this CLI ABI",
        ));
    }
    let Some(Value::Array(licenses)) = fields.get("licenses") else {
        return Err(error(
            "native HTTP client requires its two original license materials",
        ));
    };
    if licenses.len() != 2 {
        return Err(error(
            "native HTTP client requires exactly two license files",
        ));
    }
    for (value, name) in licenses.iter().zip(["LICENSE.ku", "LICENSE.dependencies"]) {
        declared_file(value, name, "", 1024 * 1024)?;
    }
    let libs = manifest::object(
        fields.get("native_static_libs").unwrap(),
        "native_static_libs",
    )?;
    manifest::keys(libs, &["raw", "tokens"], &[])?;
    let allowed: &[&str] = match target.rust_triple {
        "aarch64-apple-darwin" => &[
            "-framework",
            "CoreFoundation",
            "Security",
            "SystemConfiguration",
            "-lSystem",
            "-liconv",
            "-lc",
            "-lm",
            "-lpthread",
        ],
        "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu" => &[
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
            "-lc",
        ],
        "x86_64-pc-windows-msvc" => &[
            "dbghelp.lib",
            "/defaultlib:msvcrt",
            "advapi32.lib",
            "bcrypt.lib",
            "kernel32.lib",
            "ntdll.lib",
            "userenv.lib",
            "ws2_32.lib",
            "synchronization.lib",
            "msvcrt.lib",
            "vcruntime.lib",
            "ucrt.lib",
            "legacy_stdio_definitions.lib",
        ],
        "x86_64-pc-windows-gnu" => &[
            "-ladvapi32",
            "-lbcrypt",
            "-lkernel32",
            "-lntdll",
            "-ldbghelp",
            "-luserenv",
            "-lws2_32",
            "-lsynchronization",
            "-lmsvcrt",
            "-lgcc_eh",
            "-l:libpthread.a",
            "-lpthread",
            "-lgcc",
            "-lmingw32",
            "-lmingwex",
            "-lmoldname",
            "-lmsvcrt-os",
            "-luser32",
        ],
        _ => {
            return Err(error(
                "native HTTP client has no verified system link contract for this target",
            ))
        }
    };
    let Some(Value::Array(tokens)) = libs.get("tokens") else {
        return Err(error("native HTTP client libraries must be an array"));
    };
    let mut libraries = Vec::new();
    for token in tokens {
        let Value::String(token) = token else {
            return Err(error("invalid http_client library token"));
        };
        if !allowed.contains(&token.as_str()) {
            return Err(error(format!(
                "unverified native HTTP client system library '{token}'"
            )));
        }
        libraries.push(token.clone());
    }
    let raw = manifest::text(libs, "raw")?;
    if raw.len() > 4096
        || raw.contains(['\n', '\r', '\0'])
        || !raw
            .split_ascii_whitespace()
            .eq(libraries.iter().map(String::as_str))
    {
        return Err(error(
            "native HTTP client system libraries do not match the recorded link contract",
        ));
    }
    for (index, token) in libraries.iter().enumerate() {
        let frameworks = ["CoreFoundation", "Security", "SystemConfiguration"];
        if (token == "-framework"
            && !libraries
                .get(index + 1)
                .is_some_and(|n| frameworks.contains(&n.as_str())))
            || (frameworks.contains(&token.as_str())
                && index
                    .checked_sub(1)
                    .and_then(|i| libraries.get(i))
                    .map(String::as_str)
                    != Some("-framework"))
        {
            return Err(error("native HTTP client framework contract is invalid"));
        }
    }
    if libraries.is_empty() {
        return Err(error("native HTTP client system link contract is empty"));
    }
    let mut opened = archive
        .handle
        .try_clone()
        .map_err(|e| error(format!("cannot inspect native HTTP client archive: {e}")))?;
    native_tls_archive::validate_rust_static(
        &mut opened,
        archive.identity.length,
        format,
        &[
            b"__ku_http_client_abi_version",
            b"__ku_http_client_v1_build_id",
            b"__ku_http_client_call_v1",
            b"__ku_http_client_release_v1",
        ],
        BUILD_ID.as_bytes(),
    )
    .map_err(|e| error(format!("native HTTP client archive is invalid: {e}")))?;
    let staged = archive.stage_for_link("native HTTP client archive")?;
    if staged.sha256
        != manifest::sha256(manifest::text(
            manifest::object(fields.get("archive").unwrap(), "archive")?,
            "sha256",
        )?)?
    {
        return Err(error("native HTTP client archive changed before linking"));
    }
    Ok(StagedHttpClientInputs {
        archive: staged,
        libraries,
        msvc: target.rust_triple.ends_with("msvc"),
    })
}
