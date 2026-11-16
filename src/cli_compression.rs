//! Pinned optional native codec inputs. Resolution does not consult a system
//! search path, and unused codecs do not inspect environment variables or SDKs.
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use super::{
    link_library_identity, path_is_plain_directory, pinned_file_bytes, pinned_file_sha256,
    require_native_tls_pack_file, same_open_link_library_contents, BuildTarget, NativeBinaryFormat,
    PinnedLinkLibrary, StagedLinkLibrary, TempBuildDir,
};
use crate::{
    error::KuError,
    native_tls_archive::{self, NativeTlsArchiveFormat},
    span::Span,
    value::Value,
};

const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_HEADER: u64 = 512 * 1024;
const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
const MAX_LICENSE: u64 = 64 * 1024;

const ZSTD_SYMBOLS: &[&[u8]] = &[
    b"ZSTD_versionNumber",
    b"ZSTD_createCCtx",
    b"ZSTD_CCtx_setParameter",
    b"ZSTD_CCtx_setPledgedSrcSize",
    b"ZSTD_compressStream2",
    b"ZSTD_freeCCtx",
    b"ZSTD_createDCtx",
    b"ZSTD_initDStream",
    b"ZSTD_DCtx_setParameter",
    b"ZSTD_decompressStream",
    b"ZSTD_freeDCtx",
    b"ZSTD_isError",
    b"ZSTD_getErrorCode",
];
const ZLIB_SYMBOLS: &[&[u8]] = &[
    b"zlibVersion",
    b"deflateInit2_",
    b"inflateInit2_",
    b"deflate",
    b"inflate",
    b"inflateReset2",
    b"deflateEnd",
    b"inflateEnd",
];

#[derive(Clone, Copy)]
struct Codec {
    name: &'static str,
    version: &'static str,
    library_env: &'static str,
    include_env: &'static str,
    headers: &'static [&'static str],
    spdx: &'static str,
    symbols: &'static [&'static [u8]],
}
const ZSTD: Codec = Codec {
    name: "zstd",
    version: "1.5.7",
    library_env: "KU_ZSTD_LIB",
    include_env: "KU_ZSTD_INCLUDE",
    headers: &["zstd.h", "zstd_errors.h"],
    spdx: "BSD-3-Clause",
    symbols: ZSTD_SYMBOLS,
};
const ZLIB: Codec = Codec {
    name: "zlib",
    version: "1.4",
    library_env: "KU_ZLIB_LIB",
    include_env: "KU_ZLIB_INCLUDE",
    headers: &["zlib.h", "zconf.h"],
    spdx: "Zlib",
    symbols: ZLIB_SYMBOLS,
};

#[derive(Clone, Copy)]
struct Contract {
    target: &'static str,
    object_format: &'static str,
    format: NativeTlsArchiveFormat,
    abi: &'static str,
    crt: &'static str,
    windows: bool,
}

fn error(message: impl Into<String>) -> KuError {
    KuError::message(message)
}

fn contract(target: &BuildTarget) -> Result<Contract, KuError> {
    let (object_format, format, abi, crt) = match (target.rust_triple, target.binary_format) {
        ("x86_64-unknown-linux-gnu", NativeBinaryFormat::ElfX86_64) => (
            "elf-x86_64",
            NativeTlsArchiveFormat::ElfX86_64,
            "gnu",
            "system-dynamic",
        ),
        ("aarch64-unknown-linux-gnu", NativeBinaryFormat::ElfArm64) => (
            "elf-arm64",
            NativeTlsArchiveFormat::ElfArm64,
            "gnu",
            "system-dynamic",
        ),
        ("aarch64-apple-darwin", NativeBinaryFormat::MachOArm64) => (
            "macho-arm64",
            NativeTlsArchiveFormat::MachOArm64,
            "apple",
            "system-dynamic",
        ),
        ("x86_64-pc-windows-msvc", NativeBinaryFormat::PeX86_64) => (
            "coff-x86_64",
            NativeTlsArchiveFormat::CoffX86_64,
            "msvc",
            "msvc-dynamic",
        ),
        ("x86_64-pc-windows-gnu", NativeBinaryFormat::PeX86_64) => (
            "coff-x86_64",
            NativeTlsArchiveFormat::CoffX86_64,
            "gnu",
            "mingw-dynamic",
        ),
        _ => {
            return Err(error(format!(
                "native compression has no target contract for '{}'",
                target.rust_triple
            )))
        }
    };
    Ok(Contract {
        target: target.rust_triple,
        object_format,
        format,
        abi,
        crt,
        windows: target.is_windows,
    })
}

#[derive(Debug)]
struct VerifiedFile {
    name: String,
    pinned: PinnedLinkLibrary,
    sha256: [u8; 32],
    maximum: u64,
}

impl VerifiedFile {
    fn open(
        directory: &Path,
        declaration: &FileDecl,
        maximum: u64,
        label: &str,
    ) -> Result<Self, KuError> {
        let pinned = require_native_tls_pack_file(&directory.join(&declaration.name), label)?;
        if declaration.size == 0
            || declaration.size > maximum
            || pinned.identity.length != declaration.size
        {
            return Err(error(format!(
                "{label} length does not match the compression manifest or exceeds {maximum} bytes"
            )));
        }
        if pinned_file_sha256(&pinned, maximum, label)? != declaration.sha256 {
            return Err(error(format!(
                "{label} SHA-256 does not match the compression manifest"
            )));
        }
        Ok(Self {
            name: declaration.name.clone(),
            pinned,
            sha256: declaration.sha256,
            maximum,
        })
    }

    fn bytes(&self, label: &str) -> Result<Vec<u8>, KuError> {
        let bytes = pinned_file_bytes(&self.pinned, self.maximum, label)?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if digest != self.sha256 {
            return Err(error(format!(
                "{label} changed after compression pack validation"
            )));
        }
        Ok(bytes)
    }
}

struct CompressionPack {
    archive: VerifiedFile,
    headers: Vec<VerifiedFile>,
    manifest: VerifiedFile,
    license: VerifiedFile,
}

#[derive(Default)]
pub(super) struct CompressionInputs {
    packs: Vec<CompressionPack>,
    /// Windows callers must compare this with the selected C compiler ABI.
    pub(super) abi: Option<&'static str>,
}

pub(super) struct StagedCompressionInputs {
    pub(super) archives: Vec<StagedLinkLibrary>,
    pub(super) include_dirs: Vec<PathBuf>,
    _headers: Option<TempBuildDir>,
}

impl CompressionInputs {
    pub(super) fn stage_for_link(&self) -> Result<StagedCompressionInputs, KuError> {
        if self.packs.is_empty() {
            return Ok(StagedCompressionInputs {
                archives: Vec::new(),
                include_dirs: Vec::new(),
                _headers: None,
            });
        }
        let headers = TempBuildDir::create_private("compression-headers")
            .map_err(|e| error(format!("cannot create private compression headers: {e}")))?;
        let mut archives = Vec::with_capacity(self.packs.len());
        for pack in &self.packs {
            pack.manifest.bytes("native compression manifest")?;
            pack.license.bytes("native compression license")?;
            let archive = pack
                .archive
                .pinned
                .stage_for_link("native compression archive")?;
            if archive.sha256 != pack.archive.sha256 {
                return Err(error(
                    "compression archive changed before the private link copy",
                ));
            }
            archives.push(archive);
            for header in &pack.headers {
                let bytes = header.bytes("native compression header")?;
                let path = headers.path().join(&header.name);
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|e| {
                        error(format!(
                            "cannot stage compression header '{}': {e}",
                            path.display()
                        ))
                    })?;
                output
                    .write_all(&bytes)
                    .and_then(|_| output.flush())
                    .map_err(|e| {
                        error(format!(
                            "cannot write compression header '{}': {e}",
                            path.display()
                        ))
                    })?;
                drop(output);
                let staged = PinnedLinkLibrary::capture(&path, "staged compression header")?;
                if pinned_file_sha256(&staged, header.maximum, "staged compression header")?
                    != header.sha256
                {
                    return Err(error("private compression header failed SHA-256 readback"));
                }
            }
        }
        let include_dirs = vec![headers.path().to_path_buf()];
        Ok(StagedCompressionInputs {
            archives,
            include_dirs,
            _headers: Some(headers),
        })
    }
}

pub(super) fn resolve(
    target: &BuildTarget,
    zstd: bool,
    zlib: bool,
) -> Result<CompressionInputs, KuError> {
    resolve_with(
        target,
        zstd,
        zlib,
        |name| std::env::var_os(name),
        || {
            std::env::current_exe().map_err(|e| {
                error(format!(
                    "cannot locate Ku for optional compression SDK lookup: {e}"
                ))
            })
        },
    )
}

/// Select the SDK by the C compiler's actual effective ABI, not by the
/// runner's Rust target. In particular Zig/MinGW cannot consume an MSVC pack.
pub(super) fn compiler_target(
    requested: &BuildTarget,
    effective: Option<&str>,
    compiler: &str,
) -> Result<BuildTarget, KuError> {
    let effective = effective.ok_or_else(|| {
        error(format!(
            "cannot determine compiler '{compiler}' effective target for native compression"
        ))
    })?;
    let lower = effective.to_ascii_lowercase();
    let mut selected = requested.clone();
    let x86 = lower.contains("x86_64") || lower.contains("amd64");
    let arm = lower.contains("aarch64") || lower.contains("arm64");
    selected.rust_triple = match requested.binary_format {
        NativeBinaryFormat::ElfX86_64 | NativeBinaryFormat::ElfArm64
            if ((requested.binary_format == NativeBinaryFormat::ElfX86_64 && x86)
                || (requested.binary_format == NativeBinaryFormat::ElfArm64 && arm))
                && lower.contains("linux") && lower.contains("gnu")
                && !lower.contains("musl") && !lower.contains("android")
                && !lower.contains("uclibc") => requested.rust_triple,
        NativeBinaryFormat::MachOArm64 if arm && lower.contains("apple")
            && (lower.contains("darwin") || lower.contains("macos"))
            && !lower.contains("ios") && !lower.contains("tvos")
            && !lower.contains("watchos") => "aarch64-apple-darwin",
        NativeBinaryFormat::PeX86_64 if x86 && lower.contains("msvc") =>
            "x86_64-pc-windows-msvc",
        NativeBinaryFormat::PeX86_64 if x86
            && (lower.contains("windows-gnu") || lower.contains("mingw")) =>
            "x86_64-pc-windows-gnu",
        _ => return Err(error(format!(
            "compiler '{compiler}' effective target '{effective}' does not match a native compression pack for '{}'",
            requested.rust_triple
        ))),
    };
    // Validate the exact object/target pair before any environment/SDK lookup.
    contract(&selected)?;
    Ok(selected)
}

pub(super) fn validate_link_mode(
    used: bool,
    target: &BuildTarget,
    static_link: bool,
) -> Result<(), KuError> {
    if used && static_link && target.rust_triple == "x86_64-pc-windows-msvc" {
        return Err(error(
            "native compression cannot combine --static with the MSVC target packs: they use /MD, but --static selects /MT; a separately verified compatible CRT pack is required\nhelp: omit --static; the codecs themselves remain linked from verified static archives",
        ));
    }
    // POSIX codec archives do not themselves require dynamic libc. In Linux
    // --static keeps its existing meaning and the selected toolchain/sysroot
    // must supply static system libraries. Do not impose a new blanket ban.
    Ok(())
}

fn resolve_with(
    target: &BuildTarget,
    zstd: bool,
    zlib: bool,
    mut environment: impl FnMut(&str) -> Option<OsString>,
    mut executable: impl FnMut() -> Result<PathBuf, KuError>,
) -> Result<CompressionInputs, KuError> {
    if !zstd && !zlib {
        return Ok(CompressionInputs::default());
    }
    let contract = contract(target)?;
    let mut packs = Vec::with_capacity(usize::from(zstd) + usize::from(zlib));
    for (codec, used) in [(ZSTD, zstd), (ZLIB, zlib)] {
        if !used {
            continue;
        }
        let library = environment(codec.library_env);
        let include = environment(codec.include_env);
        let requested = if let Some(value) = library {
            absolute(value, codec.library_env)?
        } else {
            let exe = executable()?;
            let parent = exe
                .parent()
                .ok_or_else(|| error("Ku executable has no parent for compression SDK lookup"))?;
            parent
                .join("native-compression")
                .join("v1")
                .join(contract.target)
                .join(codec.name)
                .join("lib")
        };
        let library = directory(&requested, codec.library_env)?;
        let leaf = library
            .parent()
            .ok_or_else(|| error("compression lib directory has no pack parent"))?;
        if !path_is_plain_directory(leaf) {
            return Err(error(
                "native compression pack parent must be a plain directory",
            ));
        }
        let requested_include = match include {
            Some(value) => absolute(value, codec.include_env)?,
            None => leaf.join("include"),
        };
        let include = directory(&requested_include, codec.include_env)?;
        packs.push(load_pack(codec, contract, leaf, &library, &include)?);
    }
    Ok(CompressionInputs {
        packs,
        abi: Some(contract.abi),
    })
}

fn absolute(value: OsString, name: &str) -> Result<PathBuf, KuError> {
    if value.is_empty() || value.as_encoded_bytes().contains(&0) {
        return Err(error(format!("{name} must be a non-empty absolute directory without NUL bytes; no fallback is performed")));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(error(format!("{name} must be an absolute directory; relative lookup and system fallback are forbidden")));
    }
    Ok(path)
}

fn directory(path: &Path, label: &str) -> Result<PathBuf, KuError> {
    let canonical = fs::canonicalize(path)
        .map_err(|e| error(format!("cannot resolve {label} directory '{}': {e}; install a target compression pack or configure the explicit directory", path.display())))?;
    if !path_is_plain_directory(&canonical) {
        return Err(error(format!(
            "{label} '{}' must resolve to a plain directory",
            path.display()
        )));
    }
    Ok(canonical)
}

fn load_pack(
    codec: Codec,
    contract: Contract,
    leaf: &Path,
    library: &Path,
    include: &Path,
) -> Result<CompressionPack, KuError> {
    let manifest =
        require_native_tls_pack_file(&leaf.join("manifest.json"), "native compression manifest")?;
    let bytes = pinned_file_bytes(&manifest, MAX_MANIFEST, "native compression manifest")?;
    let parsed = parse_manifest(&bytes, codec, contract)?;
    let archive = VerifiedFile::open(
        library,
        &parsed.archive,
        MAX_ARCHIVE,
        "native compression archive",
    )?;
    let mut headers = Vec::with_capacity(parsed.headers.len());
    for header in &parsed.headers {
        headers.push(VerifiedFile::open(
            include,
            header,
            MAX_HEADER,
            "native compression header",
        )?);
    }
    let license = VerifiedFile::open(
        leaf,
        &parsed.license,
        MAX_LICENSE,
        "native compression license",
    )?;
    let mut reader = archive
        .pinned
        .handle
        .try_clone()
        .map_err(|e| error(format!("cannot inspect native compression archive: {e}")))?;
    native_tls_archive::validate_static(
        &mut reader,
        archive.pinned.identity.length,
        contract.format,
        codec.symbols,
    )
    .map_err(|e| {
        error(format!(
            "native compression archive failed target/static API validation: {e}"
        ))
    })?;
    let after = link_library_identity(&archive.pinned.handle).map_err(|e| {
        error(format!(
            "cannot re-identify native compression archive: {e}"
        ))
    })?;
    if !same_open_link_library_contents(&after, &archive.pinned.identity) {
        return Err(error(
            "native compression archive changed during object validation",
        ));
    }
    Ok(CompressionPack {
        archive,
        headers,
        license,
        manifest: VerifiedFile {
            name: "manifest.json".into(),
            sha256: Sha256::digest(&bytes).into(),
            pinned: manifest,
            maximum: MAX_MANIFEST,
        },
    })
}

#[derive(Debug)]
struct FileDecl {
    name: String,
    size: u64,
    sha256: [u8; 32],
}
struct Manifest {
    archive: FileDecl,
    headers: Vec<FileDecl>,
    license: FileDecl,
}

pub(super) fn object<'a>(
    value: &'a Value,
    label: &str,
) -> Result<&'a HashMap<String, Value>, KuError> {
    match value {
        Value::Object(fields) => Ok(fields),
        _ => Err(error(format!(
            "compression manifest {label} must be an object"
        ))),
    }
}
pub(super) fn text<'a>(fields: &'a HashMap<String, Value>, name: &str) -> Result<&'a str, KuError> {
    match fields.get(name) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(error(format!(
            "compression manifest '{name}' must be a string"
        ))),
    }
}
fn require(fields: &HashMap<String, Value>, name: &str, expected: &str) -> Result<(), KuError> {
    if text(fields, name)? != expected {
        return Err(error(format!(
            "compression manifest '{name}' must be '{expected}'"
        )));
    }
    Ok(())
}
pub(super) fn keys(
    fields: &HashMap<String, Value>,
    required: &[&str],
    optional: &[&str],
) -> Result<(), KuError> {
    if fields
        .keys()
        .any(|name| !required.contains(&name.as_str()) && !optional.contains(&name.as_str()))
        || required.iter().any(|name| !fields.contains_key(*name))
    {
        return Err(error(
            "compression manifest has missing or unrecognized fields",
        ));
    }
    Ok(())
}
pub(super) fn sha256(text: &str) -> Result<[u8; 32], KuError> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(error(
            "compression manifest SHA-256 must contain exactly 64 lowercase hexadecimal digits",
        ));
    }
    let mut digest = [0u8; 32];
    for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let nibble = |b: u8| {
            if b.is_ascii_digit() {
                b - b'0'
            } else {
                b - b'a' + 10
            }
        };
        digest[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Ok(digest)
}
fn file_decl(value: &Value, label: &str, allowed_names: &[&str]) -> Result<FileDecl, KuError> {
    let fields = object(value, label)?;
    keys(fields, &["name", "size", "sha256"], &[])?;
    let name = text(fields, "name")?;
    if !allowed_names.contains(&name) {
        return Err(error(format!(
            "compression manifest {label} name '{name}' is not a canonical target filename"
        )));
    }
    let size = match fields.get("size") {
        Some(Value::Int(size)) if *size > 0 => *size as u64,
        _ => {
            return Err(error(
                "compression manifest file size must be a positive integer",
            ))
        }
    };
    Ok(FileDecl {
        name: name.to_owned(),
        size,
        sha256: sha256(text(fields, "sha256")?)?,
    })
}

fn parse_manifest(bytes: &[u8], codec: Codec, contract: Contract) -> Result<Manifest, KuError> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_MANIFEST || bytes.contains(&0) {
        return Err(error(
            "native compression manifest is empty, contains NUL or exceeds 64 KiB",
        ));
    }
    reject_duplicate_keys(bytes)?;
    let json =
        std::str::from_utf8(bytes).map_err(|_| error("compression manifest is not UTF-8"))?;
    let decoded =
        crate::stdlib::json::eval("decode", &[Value::String(json.to_owned())], Span::default())?
            .ok_or_else(|| error("compression manifest JSON decoder returned no result"))?;
    let value = match decoded {
        Value::Result { ok: true, value } => value,
        _ => return Err(error("native compression manifest is not strict JSON")),
    };
    let fields = object(&value, "root")?;
    keys(
        fields,
        &[
            "format",
            "codec",
            "version",
            "target",
            "object_format",
            "abi",
            "crt",
            "archive",
            "headers",
            "runtime_dependency",
            "license",
        ],
        &["source", "build", "runtime_dependency_scope"],
    )?;
    for (name, expected) in [
        ("format", "ku-native-compression-pack-v1"),
        ("codec", codec.name),
        ("version", codec.version),
        ("target", contract.target),
        ("object_format", contract.object_format),
        ("abi", contract.abi),
        ("crt", contract.crt),
        ("runtime_dependency", "none"),
    ] {
        require(fields, name, expected)?;
    }
    if fields.contains_key("runtime_dependency_scope") {
        require(
            fields,
            "runtime_dependency_scope",
            "codec shared-library sidecars; system CRT remains required",
        )?;
    }
    let names: &[&str] = match (codec.name, contract.windows, contract.abi) {
        ("zstd", true, "msvc") => &["zstd_static.lib", "libzstd_static.lib"],
        ("zlib", true, "msvc") => &["libzs.lib"],
        ("zstd", _, _) => &["libzstd.a"],
        _ => &["libz.a"],
    };
    let archive = file_decl(&fields["archive"], "archive", names)?;
    let values = match &fields["headers"] {
        Value::Array(values) if values.len() == codec.headers.len() => values,
        _ => {
            return Err(error(
                "compression manifest header list does not match the selected codec",
            ))
        }
    };
    let mut headers = Vec::with_capacity(values.len());
    let mut found = HashSet::new();
    for value in values {
        let declaration = file_decl(value, "header", codec.headers)?;
        if !found.insert(declaration.name.clone()) {
            return Err(error("compression manifest duplicates a header"));
        }
        headers.push(declaration);
    }
    let license_fields = object(&fields["license"], "license")?;
    keys(license_fields, &["name", "size", "sha256", "spdx"], &[])?;
    require(license_fields, "spdx", codec.spdx)?;
    let mut license_value = license_fields.clone();
    license_value.remove("spdx");
    let license = file_decl(&Value::Object(license_value), "license", &["LICENSE"])?;
    if let Some(build) = fields.get("build") {
        let build = object(build, "build")?;
        require(build, "profile", "Release")?;
        require(build, "optimization", "O2")?;
        if !matches!(build.get("multithread"), Some(Value::Bool(false))) {
            return Err(error(
                "compression manifest build must declare multithread=false",
            ));
        }
    }
    Ok(Manifest {
        archive,
        headers,
        license,
    })
}

/// The existing language JSON decoder deliberately accepts duplicate keys.
/// Pack metadata instead requires unescaped ASCII keys and rejects duplicates
/// before decoding, including in optional provenance objects. Values retain
/// ordinary JSON escaping. This scanner does not replace JSON syntax checking.
pub(super) fn reject_duplicate_keys(bytes: &[u8]) -> Result<(), KuError> {
    enum Container {
        Object(HashSet<String>),
        Array,
    }
    let mut stack = Vec::new();
    let mut index = 0;
    let mut keys_seen = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                if stack.len() >= 16 {
                    return Err(error("compression manifest nesting exceeds 16"));
                }
                stack.push(if bytes[index] == b'{' {
                    Container::Object(HashSet::new())
                } else {
                    Container::Array
                });
                index += 1;
            }
            b'}' => {
                if !matches!(stack.pop(), Some(Container::Object(_))) {
                    return Err(error("compression manifest has mismatched objects"));
                }
                index += 1;
            }
            b']' => {
                if !matches!(stack.pop(), Some(Container::Array)) {
                    return Err(error("compression manifest has mismatched arrays"));
                }
                index += 1;
            }
            b'"' => {
                index += 1;
                let start = index;
                let mut escaped = false;
                while index < bytes.len() && bytes[index] != b'"' {
                    if bytes[index] == b'\\' {
                        escaped = true;
                        index += 1;
                    }
                    index += 1;
                }
                if index >= bytes.len() {
                    return Err(error("compression manifest has an unterminated string"));
                }
                let end = index;
                index += 1;
                let mut next = index;
                while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                if bytes.get(next) == Some(&b':') {
                    if escaped || end == start || end - start > 64 || !bytes[start..end].is_ascii()
                    {
                        return Err(error("compression manifest keys must be unescaped ASCII names of at most 64 bytes"));
                    }
                    let key = std::str::from_utf8(&bytes[start..end])
                        .map_err(|_| error("invalid compression manifest key"))?;
                    match stack.last_mut() {
                        Some(Container::Object(keys)) => {
                            if !keys.insert(key.to_owned()) {
                                return Err(error("compression manifest has a duplicate key"));
                            }
                        }
                        _ => return Err(error("compression manifest has a misplaced key")),
                    }
                    keys_seen += 1;
                    if keys_seen > 1024 {
                        return Err(error("compression manifest exceeds its field budget"));
                    }
                }
            }
            _ => index += 1,
        }
    }
    if !stack.is_empty() {
        return Err(error("compression manifest has unfinished containers"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> BuildTarget {
        super::super::resolve_build_target(Some("aarch64-darwin"))
            .unwrap()
            .unwrap()
    }

    fn fixture() -> String {
        let hash = "a".repeat(64);
        format!(
            r#"{{"format":"ku-native-compression-pack-v1","codec":"zstd","version":"1.5.7","target":"aarch64-apple-darwin","object_format":"macho-arm64","abi":"apple","crt":"system-dynamic","archive":{{"name":"libzstd.a","size":123,"sha256":"{hash}"}},"headers":[{{"name":"zstd.h","size":20,"sha256":"{hash}"}},{{"name":"zstd_errors.h","size":10,"sha256":"{hash}"}}],"runtime_dependency":"none","license":{{"name":"LICENSE","size":30,"sha256":"{hash}","spdx":"BSD-3-Clause"}},"build":{{"profile":"Release","optimization":"O2","multithread":false}}}}"#
        )
    }

    #[test]
    fn unused_compression_never_reads_configuration_or_creates_staging() {
        let input = resolve_with(
            &target(),
            false,
            false,
            |_| panic!("unused codec read env"),
            || panic!("unused codec looked up SDK"),
        )
        .unwrap();
        let staged = input.stage_for_link().unwrap();
        assert!(
            input.abi.is_none()
                && staged.archives.is_empty()
                && staged.include_dirs.is_empty()
                && staged._headers.is_none()
        );
    }

    #[test]
    fn compression_pack_tracks_actual_compiler_abi_and_static_crt_contract() {
        let windows = super::super::resolve_build_target(Some("x86_64-windows"))
            .unwrap()
            .unwrap();
        let gnu = compiler_target(&windows, Some("x86_64-w64-mingw32"), "gcc").unwrap();
        assert_eq!(gnu.rust_triple, "x86_64-pc-windows-gnu");
        assert_eq!(contract(&gnu).unwrap().abi, "gnu");
        let msvc = compiler_target(&windows, Some("x86_64-pc-windows-msvc"), "clang").unwrap();
        assert_eq!(contract(&msvc).unwrap().abi, "msvc");
        assert!(validate_link_mode(true, &msvc, true)
            .unwrap_err()
            .to_string()
            .contains("/MD"));
        validate_link_mode(false, &msvc, true).unwrap();
        validate_link_mode(true, &gnu, true).unwrap();
        let linux = super::super::resolve_build_target(Some("x86_64-linux"))
            .unwrap()
            .unwrap();
        let linux = compiler_target(&linux, Some("x86_64-linux-gnu"), "gcc").unwrap();
        validate_link_mode(true, &linux, true).unwrap();
        for bad in [
            None,
            Some("aarch64-linux-gnu"),
            Some("x86_64-linux-musl"),
            Some("x86_64-unknown-linux-android"),
        ] {
            assert!(compiler_target(&linux, bad, "test compiler").is_err());
        }
        assert!(compiler_target(&windows, Some("x86_64-unknown-linux-gnu"), "gcc").is_err());
    }

    #[test]
    fn compression_manifest_contracts_reject_ambiguity_and_path_escape() {
        let valid = fixture();
        let expected = contract(&target()).unwrap();
        assert!(parse_manifest(valid.as_bytes(), ZSTD, expected).is_ok());
        let with_scope = valid.replace(
            "\"runtime_dependency\":\"none\"",
            "\"runtime_dependency\":\"none\",\"runtime_dependency_scope\":\"codec shared-library sidecars; system CRT remains required\"",
        );
        assert!(parse_manifest(with_scope.as_bytes(), ZSTD, expected).is_ok());
        assert!(parse_manifest(
            with_scope
                .replace("system CRT remains required", "all libraries bundled")
                .as_bytes(),
            ZSTD,
            expected,
        )
        .is_err());
        for bad in [
            valid.replace(
                "\"version\":\"1.5.7\"",
                "\"version\":\"1.5.7\",\"version\":\"1.5.6\"",
            ),
            valid.replace("\"version\"", "\"v\\u0065rsion\""),
            valid.replace("libzstd.a", "../libzstd.a"),
            valid.replace("macho-arm64", "elf-x86_64"),
            valid.replace("\"abi\":\"apple\"", "\"abi\":\"gnu\""),
            valid.replace(
                "\"runtime_dependency\":\"none\"",
                "\"runtime_dependency\":\"zstd.dll\"",
            ),
            valid.replace("\"size\":123", "\"size\":123.0"),
            valid.replace("\"multithread\":false", "\"multithread\":true"),
            valid.replace("zstd_errors.h", "zstd.h"),
            valid.replace("BSD-3-Clause", "GPL-2.0"),
        ] {
            assert!(
                parse_manifest(bad.as_bytes(), ZSTD, expected).is_err(),
                "accepted {bad}"
            );
        }
        assert!(parse_manifest(&vec![b' '; MAX_MANIFEST as usize + 1], ZSTD, expected).is_err());
    }

    #[test]
    fn explicit_compression_directories_do_not_fall_back() {
        for configured in ["", "relative/lib", "/absolute/contains\0nul"] {
            let result = resolve_with(
                &target(),
                true,
                false,
                |name| (name == "KU_ZSTD_LIB").then(|| configured.into()),
                || panic!("explicit invalid directory fell back to executable"),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn pinned_compression_headers_reject_mutation_and_keep_original_file_identity() {
        let directory = TempBuildDir::create_private("codec-header-test").unwrap();
        let path = directory.path().join("zstd.h");
        fs::write(&path, b"original header").unwrap();
        let declaration = FileDecl {
            name: "zstd.h".into(),
            size: 15,
            sha256: Sha256::digest(b"original header").into(),
        };
        let header =
            VerifiedFile::open(directory.path(), &declaration, MAX_HEADER, "header test").unwrap();
        fs::rename(&path, directory.path().join("kept-header")).unwrap();
        fs::write(&path, b"replaced header").unwrap();
        assert_eq!(header.bytes("pinned header").unwrap(), b"original header");
        fs::write(directory.path().join("kept-header"), b"modified header").unwrap();
        assert!(header.bytes("changed pinned header").is_err());
    }
}
