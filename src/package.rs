use std::{
    collections::HashMap,
    env, fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    error::{KuError, KuResult},
    span::Span,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KuMod {
    pub name: String,
    pub version: Option<String>,
    pub root: Option<String>,
    pub main: Option<String>,
    pub out: Option<String>,
    pub cache: Option<String>,
    pub template: Option<String>,
    pub package_type: Option<String>,
    pub dependencies: Vec<PackageDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDependency {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageContext {
    pub manifest_path: PathBuf,
    pub package_dir: PathBuf,
    pub import_root: PathBuf,
    pub cache_dir: PathBuf,
    pub lock_path: PathBuf,
    pub manifest: KuMod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockDependency {
    pub path: PathBuf,
    pub cache_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryManifest {
    pub name: String,
    pub version: String,
    pub source: String,
    pub checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryIndex {
    pub name: String,
    pub versions: Vec<RegistryManifest>,
}

pub trait RegistryIndexVerifier {
    fn verify(&self, index_url: &str, index_bytes: &[u8], span: Span) -> KuResult<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryLockPackage {
    pub name: String,
    pub version: String,
    pub source: String,
    pub url: String,
    pub checksum: String,
    pub cache_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionRequirement {
    Exact(PackageVersion),
    Caret(PackageVersion),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryFetchPolicy {
    pub max_attempts: u8,
    pub connect_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub max_download_bytes: u64,
}

impl Default for RegistryFetchPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            connect_timeout_ms: 10_000,
            read_timeout_ms: 30_000,
            max_download_bytes: 50_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryCacheAction {
    ReuseVerified,
    DownloadAndReplace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryDownloadPlan {
    pub name: String,
    pub version: String,
    pub url: String,
    pub checksum: String,
    pub target_dir: PathBuf,
    pub temporary_dir: PathBuf,
    pub action: RegistryCacheAction,
    pub policy: RegistryFetchPolicy,
}

pub const MANIFEST_FILE: &str = "ku.mod";
pub const LOCK_FILE: &str = "ku.lock";
pub const DEFAULT_IMPORT_ROOT: &str = "src";
pub const DEFAULT_MAIN_FILE: &str = "main.ku";
pub const DEFAULT_BUILD_DIR: &str = ".ku/build";
pub const DEFAULT_CACHE_DIR: &str = ".ku/cache";
const PACKAGE_CACHE_DIR: &str = "packages";
const MAX_PACKAGE_BYTES: u64 = 10_000_000;
const MAX_PACKAGE_FILES: usize = 512;
const MAX_REGISTRY_FETCH_ATTEMPTS: u8 = 8;
const MAX_REGISTRY_DOWNLOAD_BYTES: u64 = 100_000_000;
const MAX_REGISTRY_TIMEOUT_MS: u64 = 300_000;
const REGISTRY_ARTIFACT_FILE: &str = "package.archive";
const REGISTRY_CHECKSUM_FILE: &str = ".sha256";
const REGISTRY_INSTALL_LOCK_ATTEMPTS: u8 = 100;
const REGISTRY_INSTALL_LOCK_DELAY_MS: u64 = 10;
const REGISTRY_INSTALL_LOCK_STALE_SECS: u64 = 30;
static NEXT_REGISTRY_DOWNLOAD_ID: AtomicU64 = AtomicU64::new(1);

pub fn discover_for_file(path: &Path) -> KuResult<Option<PackageContext>> {
    let start = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    discover_from_dir(start)
}

pub fn discover_from_dir(start: &Path) -> KuResult<Option<PackageContext>> {
    let mut current = fs::canonicalize(start).map_err(|err| {
        KuError::message(format!(
            "failed to resolve package start '{}': {err}",
            start.display()
        ))
    })?;
    loop {
        let manifest_path = current.join(MANIFEST_FILE);
        if manifest_path.exists() {
            let source = fs::read_to_string(&manifest_path).map_err(|err| {
                KuError::message(format!(
                    "failed to read package manifest '{}': {err}",
                    manifest_path.display()
                ))
            })?;
            let manifest = parse_manifest(&source, Span::default())?;
            let import_root = current.join(manifest.root.as_deref().unwrap_or(DEFAULT_IMPORT_ROOT));
            let import_root = if import_root.exists() {
                fs::canonicalize(&import_root).map_err(|err| {
                    KuError::message(format!(
                        "failed to resolve package import root '{}': {err}",
                        import_root.display()
                    ))
                })?
            } else {
                import_root
            };
            let cache_dir = current.join(manifest.cache.as_deref().unwrap_or(DEFAULT_CACHE_DIR));
            return Ok(Some(PackageContext {
                manifest_path,
                package_dir: current.clone(),
                import_root,
                cache_dir,
                lock_path: current.join(LOCK_FILE),
                manifest,
            }));
        }
        if !current.pop() {
            return Ok(None);
        }
    }
}

pub fn default_global_cache() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("ku")
        .join("cache")
}

pub fn ensure_cache_dir(package: &PackageContext) -> KuResult<()> {
    fs::create_dir_all(&package.cache_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create package cache '{}': {err}",
            package.cache_dir.display()
        ))
    })
}

pub fn write_lock(package: &PackageContext) -> KuResult<()> {
    write_lock_with_dependencies(package, &[])
}

pub fn write_lock_with_dependencies(
    package: &PackageContext,
    dependencies: &[PathBuf],
) -> KuResult<()> {
    let version = package.manifest.version.as_deref().unwrap_or("0.0.0");
    let mut source = format!(
        "# generated by ku\npackage = {:?}\nversion = {:?}\nroot = {:?}\nmain = {:?}\nout = {:?}\ncache = {:?}\n",
        package.manifest.name,
        version,
        package
            .manifest
            .root
            .as_deref()
            .unwrap_or(DEFAULT_IMPORT_ROOT),
        package
            .manifest
            .main
            .as_deref()
            .unwrap_or(DEFAULT_MAIN_FILE),
        package
            .manifest
            .out
            .as_deref()
            .unwrap_or(DEFAULT_BUILD_DIR),
        package
            .manifest
            .cache
            .as_deref()
            .unwrap_or(DEFAULT_CACHE_DIR)
    );
    for dependency in lock_dependencies(package, dependencies)? {
        source.push_str(&format!(
            "\n[[dependency]]\npath = {:?}\ncache_key = {:?}\n",
            display_dependency_path(package, &dependency.path),
            dependency.cache_key
        ));
    }
    for dependency in &package.manifest.dependencies {
        source.push_str(&format!(
            "\n[[package_dependency]]\nname = {:?}\nversion = {:?}\ncache = {:?}\n",
            dependency.name,
            dependency.version,
            dependency_cache_root(package, dependency)
                .display()
                .to_string()
                .replace('\\', "/")
        ));
        if let Some(dep_source) = &dependency.source {
            source.push_str(&format!("source = {:?}\n", dep_source));
        }
        if let Some(checksum) = &dependency.checksum {
            source.push_str(&format!("checksum = {:?}\n", checksum));
        }
    }
    fs::write(&package.lock_path, source).map_err(|err| {
        KuError::message(format!(
            "failed to write package lock '{}': {err}",
            package.lock_path.display()
        ))
    })
}

pub fn resolve_remote_dependencies(package: &PackageContext) -> KuResult<()> {
    ensure_cache_dir(package)?;
    for dependency in &package.manifest.dependencies {
        let Some(source) = &dependency.source else {
            continue;
        };
        let source_path = file_url_path(source).ok_or_else(|| {
            KuError::package("unsupported_source", format!(
                "dependency '{}' uses unsupported source '{}'; only file:// is supported in this stage",
                dependency.name, source
            ), Span::default())
        })?;
        let source_path = fs::canonicalize(&source_path).map_err(|err| {
            KuError::message(format!(
                "failed to resolve dependency '{}' source '{}': {err}",
                dependency.name,
                source_path.display()
            ))
        })?;
        let actual_checksum = package_source_checksum(&source_path)?;
        if let Some(expected) = &dependency.checksum {
            if expected != &actual_checksum {
                return Err(KuError::package(
                    "checksum_mismatch",
                    format!(
                        "dependency '{}' checksum mismatch: expected {}, got {}",
                        dependency.name, expected, actual_checksum
                    ),
                    Span::default(),
                ));
            }
        }
        let target = dependency_cache_root(package, dependency);
        if target.exists() && package_source_checksum(&target)? == actual_checksum {
            continue;
        }
        if target.exists() {
            fs::remove_dir_all(&target).map_err(|err| {
                KuError::message(format!(
                    "failed to refresh dependency cache '{}': {err}",
                    target.display()
                ))
            })?;
        }
        let tmp = target.with_extension("tmp");
        if tmp.exists() {
            fs::remove_dir_all(&tmp).map_err(|err| {
                KuError::message(format!(
                    "failed to clear dependency temp cache '{}': {err}",
                    tmp.display()
                ))
            })?;
        }
        fs::create_dir_all(&tmp).map_err(|err| {
            KuError::message(format!(
                "failed to create dependency temp cache '{}': {err}",
                tmp.display()
            ))
        })?;
        copy_package_source(&source_path, &tmp)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                KuError::message(format!(
                    "failed to create dependency cache '{}': {err}",
                    parent.display()
                ))
            })?;
        }
        fs::rename(&tmp, &target).map_err(|err| {
            KuError::message(format!(
                "failed to install dependency cache '{}': {err}",
                target.display()
            ))
        })?;
    }
    Ok(())
}

pub(crate) fn resolve_dependency_import(
    package: &PackageContext,
    import_path: &str,
    span: Span,
) -> KuResult<Option<PathBuf>> {
    let Some(rest) = import_path.strip_prefix('@') else {
        return Ok(None);
    };
    let Some((name, relative)) = rest.split_once('/') else {
        return Err(KuError::package(
            "invalid_import",
            "package dependency import must use @name/path",
            span,
        ));
    };
    let dependency = package
        .manifest
        .dependencies
        .iter()
        .find(|dep| dep.name == name)
        .ok_or_else(|| {
            KuError::package(
                "unknown_dependency",
                format!("unknown package dependency '{name}'"),
                span,
            )
        })?;
    reject_unsafe_dependency_import(relative, span)?;
    let root = dependency_cache_root(package, dependency).join(dependency.root());
    let mut path = root.join(relative);
    if path.extension().is_none() {
        path.set_extension("ku");
    }
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ku"))
    {
        return Err(KuError::runtime(
            "package dependency import must point to a .ku file",
            span,
        ));
    }
    if path.exists() {
        let canonical_root = fs::canonicalize(&root).map_err(|err| {
            KuError::runtime(
                format!(
                    "failed to resolve package dependency root '{}': {err}",
                    root.display()
                ),
                span,
            )
        })?;
        let canonical_path = fs::canonicalize(&path).map_err(|err| {
            KuError::runtime(
                format!(
                    "failed to resolve package dependency import '{}': {err}",
                    path.display()
                ),
                span,
            )
        })?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(KuError::runtime(
                "package dependency import is outside dependency root",
                span,
            ));
        }
    }
    Ok(Some(path))
}

pub fn package_source_checksum(path: &Path) -> KuResult<String> {
    let mut bytes = Vec::new();
    let mut files = 0;
    let mut total_bytes = 0;
    collect_source_bytes(path, path, &mut bytes, &mut files, &mut total_bytes)?;
    Ok(format!("ku-fnv64-{:016x}", stable_hash(&bytes)))
}

pub fn gc_cache(package: &PackageContext, max_entries: usize) -> KuResult<usize> {
    let packages_dir = package.cache_dir.join(PACKAGE_CACHE_DIR);
    if !packages_dir.exists() {
        return Ok(0);
    }
    let keep = package
        .manifest
        .dependencies
        .iter()
        .map(|dependency| dependency_cache_root(package, dependency))
        .collect::<Vec<_>>();
    let mut removed = 0;
    for name_entry in fs::read_dir(&packages_dir).map_err(|err| {
        KuError::message(format!(
            "failed to read package cache '{}': {err}",
            packages_dir.display()
        ))
    })? {
        let name_entry = name_entry
            .map_err(|err| KuError::message(format!("failed to read cache entry: {err}")))?;
        let name_path = name_entry.path();
        if !name_path.is_dir() {
            continue;
        }
        for version_entry in fs::read_dir(&name_path).map_err(|err| {
            KuError::message(format!(
                "failed to read package cache '{}': {err}",
                name_path.display()
            ))
        })? {
            let version_entry = version_entry
                .map_err(|err| KuError::message(format!("failed to read cache entry: {err}")))?;
            let path = version_entry.path();
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !path.is_dir()
                || keep.contains(&path)
                || file_name.contains(".download-")
                || file_name.contains(".replaced-")
                || name_path.join(format!("{file_name}.install.lock")).exists()
            {
                continue;
            }
            if removed >= max_entries {
                break;
            }
            fs::remove_dir_all(&path).map_err(|err| {
                KuError::message(format!(
                    "failed to remove cache '{}': {err}",
                    path.display()
                ))
            })?;
            removed += 1;
        }
        if removed >= max_entries {
            break;
        }
        if fs::read_dir(&name_path)
            .map_err(|err| {
                KuError::message(format!(
                    "failed to read package cache '{}': {err}",
                    name_path.display()
                ))
            })?
            .next()
            .is_none()
        {
            fs::remove_dir(&name_path).map_err(|err| {
                KuError::message(format!(
                    "failed to remove empty cache '{}': {err}",
                    name_path.display()
                ))
            })?;
        }
    }
    Ok(removed)
}

pub fn lock_dependencies(
    package: &PackageContext,
    dependencies: &[PathBuf],
) -> KuResult<Vec<LockDependency>> {
    let mut output = Vec::new();
    for path in dependencies {
        let bytes = fs::read(path).map_err(|err| {
            KuError::message(format!(
                "failed to read dependency '{}': {err}",
                path.display()
            ))
        })?;
        output.push(LockDependency {
            path: path.clone(),
            cache_key: format!("ku-fnv64-{:016x}", stable_hash(&bytes)),
        });
    }
    output.sort_by(|left, right| {
        display_dependency_path(package, &left.path)
            .cmp(&display_dependency_path(package, &right.path))
    });
    output.dedup_by(|left, right| left.path == right.path);
    Ok(output)
}

fn display_dependency_path(package: &PackageContext, path: &Path) -> String {
    path.strip_prefix(&package.package_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn parse_manifest(source: &str, span: Span) -> KuResult<KuMod> {
    let mut name = None;
    let mut version = None;
    let mut root = None;
    let mut main = None;
    let mut out = None;
    let mut cache = None;
    let mut template = None;
    let mut package_type = None;
    let mut dependencies = HashMap::<String, PackageDependencyDraft>::new();
    for (index, raw_line) in source.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(KuError::package(
                "invalid_manifest_line",
                format!("invalid ku.mod line {}: expected key = value", index + 1),
                span,
            ));
        };
        let key = key.trim();
        let value = parse_string_value(value.trim(), index + 1, span)?;
        match key {
            "name" => name = Some(value),
            "version" => version = Some(value),
            "root" => root = Some(value),
            "main" => main = Some(value),
            "out" => out = Some(value),
            "cache" => cache = Some(value),
            "template" => template = Some(value),
            "type" => package_type = Some(value),
            key if key.starts_with("dep.") => {
                parse_dependency_key(key, value, &mut dependencies, index + 1, span)?;
            }
            _ => {
                return Err(KuError::package(
                    "invalid_manifest_key",
                    format!("invalid ku.mod key '{key}' on line {}", index + 1),
                    span,
                ));
            }
        }
    }
    let name =
        name.ok_or_else(|| KuError::package("missing_name", "ku.mod missing package name", span))?;
    validate_package_name(&name, span)?;
    if let Some(value) = &version {
        validate_version(value, span)?;
    }
    if let Some(value) = &root {
        reject_unsafe_relative_path("root", value, span)?;
    }
    if let Some(value) = &main {
        reject_unsafe_relative_path("main", value, span)?;
    }
    if let Some(value) = &out {
        reject_unsafe_relative_path("out", value, span)?;
    }
    if let Some(value) = &cache {
        reject_unsafe_relative_path("cache", value, span)?;
    }
    if let Some(value) = &template {
        validate_manifest_label("template", value, span)?;
    }
    if let Some(value) = &package_type {
        validate_manifest_label("type", value, span)?;
    }
    let mut dependencies = dependencies
        .into_values()
        .map(|dependency| dependency.finish(span))
        .collect::<KuResult<Vec<_>>>()?;
    dependencies.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(KuMod {
        name,
        version,
        root,
        main,
        out,
        cache,
        template,
        package_type,
        dependencies,
    })
}

pub fn parse_registry_manifest(source: &str, span: Span) -> KuResult<RegistryManifest> {
    let fields = parse_flat_string_fields(source, "registry manifest", span)?;
    reject_unknown_fields(
        &fields,
        &["name", "version", "source", "checksum"],
        "registry manifest",
        span,
    )?;
    let name = required_field(&fields, "name", "registry manifest", span)?;
    let version = required_field(&fields, "version", "registry manifest", span)?;
    let source = required_field(&fields, "source", "registry manifest", span)?;
    let checksum = required_field(&fields, "checksum", "registry manifest", span)?;
    validate_package_name(&name, span)?;
    validate_version(&version, span)?;
    validate_registry_url(&source, span)?;
    validate_sha256_checksum(&checksum, span)?;
    Ok(RegistryManifest {
        name,
        version,
        source,
        checksum,
    })
}

pub fn parse_registry_index(source: &str, index_url: &str, span: Span) -> KuResult<RegistryIndex> {
    validate_registry_url(index_url, span)?;
    let mut name = None;
    let mut versions = Vec::new();
    let mut current = None::<HashMap<String, String>>;
    for (index, raw_line) in source.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[version]]" {
            if let Some(fields) = current.take() {
                versions.push(finish_registry_index_version(
                    name.as_deref(),
                    fields,
                    index_url,
                    span,
                )?);
            }
            current = Some(HashMap::new());
            continue;
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            return Err(KuError::package(
                "invalid_registry_index",
                format!(
                    "invalid registry index line {}: expected key = value",
                    index + 1
                ),
                span,
            ));
        };
        let key = key.trim();
        let value = parse_string_value(raw_value.trim(), index + 1, span)?;
        if let Some(fields) = current.as_mut() {
            if fields.insert(key.to_string(), value).is_some() {
                return Err(KuError::package(
                    "duplicate_registry_index_field",
                    format!("duplicate registry index version field '{key}'"),
                    span,
                ));
            }
        } else if key == "name" && name.is_none() {
            name = Some(value);
        } else {
            return Err(KuError::package(
                "invalid_registry_index",
                "registry index must contain one name before [[version]] entries",
                span,
            ));
        }
    }
    if let Some(fields) = current {
        versions.push(finish_registry_index_version(
            name.as_deref(),
            fields,
            index_url,
            span,
        )?);
    }
    let name = name.ok_or_else(|| {
        KuError::package(
            "missing_registry_field",
            "registry index missing required field 'name'",
            span,
        )
    })?;
    validate_package_name(&name, span)?;
    if versions.is_empty() {
        return Err(KuError::package(
            "empty_registry_index",
            "registry index must contain at least one [[version]] entry",
            span,
        ));
    }
    versions.sort_by_key(|manifest| {
        std::cmp::Reverse(
            parse_package_version(&manifest.version, span)
                .expect("validated registry index version"),
        )
    });
    for pair in versions.windows(2) {
        if pair[0].version == pair[1].version {
            return Err(KuError::package(
                "duplicate_registry_version",
                format!(
                    "registry index contains duplicate version '{}'",
                    pair[0].version
                ),
                span,
            ));
        }
    }
    Ok(RegistryIndex { name, versions })
}

pub fn fetch_registry_index(
    index_url: &str,
    verifier: &dyn RegistryIndexVerifier,
    policy: RegistryFetchPolicy,
    span: Span,
) -> KuResult<RegistryIndex> {
    validate_registry_fetch_policy(policy, span)?;
    validate_registry_url(index_url, span)?;
    let bytes = fetch_https_bytes(index_url, policy, span)?;
    verifier.verify(index_url, &bytes, span)?;
    let source = std::str::from_utf8(&bytes).map_err(|_| {
        KuError::package(
            "invalid_registry_index",
            "registry index must be valid UTF-8",
            span,
        )
    })?;
    parse_registry_index(source, index_url, span)
}

pub fn reject_unconfigured_registry_index_trust(span: Span) -> KuError {
    KuError::package(
        "registry_trust_unconfigured",
        "registry index verification requires a configured signature algorithm and trusted public key source",
        span,
    )
}

pub fn parse_package_version(version: &str, span: Span) -> KuResult<PackageVersion> {
    let mut parts = version.split('.');
    let (Some(major), Some(minor), Some(patch), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid_version_error(span));
    };
    if [major, minor, patch]
        .iter()
        .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(invalid_version_error(span));
    }
    let parse_part = |part: &str| part.parse::<u64>().map_err(|_| invalid_version_error(span));
    Ok(PackageVersion {
        major: parse_part(major)?,
        minor: parse_part(minor)?,
        patch: parse_part(patch)?,
    })
}

pub fn parse_version_requirement(requirement: &str, span: Span) -> KuResult<VersionRequirement> {
    if let Some(version) = requirement.strip_prefix('^') {
        if version.starts_with('^') {
            return Err(invalid_version_requirement_error(span));
        }
        return parse_package_version(version, span)
            .map(VersionRequirement::Caret)
            .map_err(|_| invalid_version_requirement_error(span));
    }
    parse_package_version(requirement, span)
        .map(VersionRequirement::Exact)
        .map_err(|_| invalid_version_requirement_error(span))
}

pub fn version_requirement_matches(
    requirement: VersionRequirement,
    version: PackageVersion,
) -> bool {
    match requirement {
        VersionRequirement::Exact(expected) => version == expected,
        VersionRequirement::Caret(minimum) if minimum.major > 0 => {
            version >= minimum && version.major == minimum.major
        }
        VersionRequirement::Caret(minimum) if minimum.minor > 0 => {
            version >= minimum && version.major == 0 && version.minor == minimum.minor
        }
        VersionRequirement::Caret(minimum) => {
            version >= minimum
                && version.major == 0
                && version.minor == 0
                && version.patch == minimum.patch
        }
    }
}

pub fn resolve_registry_dependencies(
    requirements: &[PackageDependency],
    manifests: &[RegistryManifest],
    span: Span,
) -> KuResult<Vec<RegistryManifest>> {
    let mut grouped = HashMap::<String, Vec<VersionRequirement>>::new();
    for dependency in requirements {
        validate_package_name(&dependency.name, span)?;
        let requirement = parse_version_requirement(&dependency.version, span)?;
        grouped
            .entry(dependency.name.clone())
            .or_default()
            .push(requirement);
    }

    let mut names = grouped.keys().cloned().collect::<Vec<_>>();
    names.sort();
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        let constraints = &grouped[&name];
        let mut candidates = manifests
            .iter()
            .filter(|manifest| manifest.name == name)
            .map(|manifest| {
                parse_package_version(&manifest.version, span).map(|version| (version, manifest))
            })
            .collect::<KuResult<Vec<_>>>()?;
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));

        let (selected_version, selected) = candidates
            .iter()
            .find(|(version, _)| {
                constraints
                    .iter()
                    .all(|requirement| version_requirement_matches(*requirement, *version))
            })
            .map(|(version, manifest)| (*version, *manifest))
            .ok_or_else(|| {
                KuError::package(
                    "dependency_conflict",
                    format!(
                        "dependency '{name}' has no registry version satisfying all requirements"
                    ),
                    span,
                )
            })?;
        if candidates
            .iter()
            .any(|(version, manifest)| *version == selected_version && *manifest != selected)
        {
            return Err(KuError::package(
                "registry_metadata_conflict",
                format!(
                    "registry contains conflicting metadata for '{}@{}'",
                    selected.name, selected.version
                ),
                span,
            ));
        }
        resolved.push(selected.clone());
    }
    Ok(resolved)
}

pub fn plan_registry_download(
    cache_dir: &Path,
    manifest: &RegistryManifest,
    cached_checksum: Option<&str>,
    policy: RegistryFetchPolicy,
    span: Span,
) -> KuResult<RegistryDownloadPlan> {
    validate_package_name(&manifest.name, span)?;
    parse_package_version(&manifest.version, span)?;
    validate_registry_url(&manifest.source, span)?;
    validate_sha256_checksum(&manifest.checksum, span)?;
    validate_registry_fetch_policy(policy, span)?;

    let target_dir = cache_dir
        .join(PACKAGE_CACHE_DIR)
        .join(&manifest.name)
        .join(registry_cache_key(manifest));
    let download_id = NEXT_REGISTRY_DOWNLOAD_ID.fetch_add(1, Ordering::Relaxed);
    let temporary_dir = cache_dir.join(".registry-downloads").join(format!(
        "{}-{}-{}-{download_id}",
        manifest.name,
        manifest.version,
        std::process::id()
    ));
    let action = if cached_checksum == Some(manifest.checksum.as_str()) {
        RegistryCacheAction::ReuseVerified
    } else {
        RegistryCacheAction::DownloadAndReplace
    };
    Ok(RegistryDownloadPlan {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        url: manifest.source.clone(),
        checksum: manifest.checksum.clone(),
        target_dir,
        temporary_dir,
        action,
        policy,
    })
}

pub fn execute_registry_download(plan: &RegistryDownloadPlan, span: Span) -> KuResult<PathBuf> {
    validate_registry_fetch_policy(plan.policy, span)?;
    validate_registry_url(&plan.url, span)?;
    validate_sha256_checksum(&plan.checksum, span)?;
    let artifact = plan.target_dir.join(REGISTRY_ARTIFACT_FILE);
    if artifact.is_file()
        && sha256_file_matches(&artifact, &plan.checksum, plan.policy.max_download_bytes)?
    {
        return Ok(artifact);
    }

    let parent = plan.target_dir.parent().ok_or_else(|| {
        KuError::package(
            "invalid_cache_path",
            "registry cache target must have a parent directory",
            span,
        )
    })?;
    fs::create_dir_all(parent).map_err(|err| {
        KuError::message(format!(
            "failed to create registry cache '{}': {err}",
            parent.display()
        ))
    })?;
    if let Some(temporary_parent) = plan.temporary_dir.parent() {
        fs::create_dir_all(temporary_parent).map_err(|err| {
            KuError::message(format!(
                "failed to create registry download staging '{}': {err}",
                temporary_parent.display()
            ))
        })?;
    }
    if plan.temporary_dir.exists() {
        return Err(KuError::package(
            "registry_temp_collision",
            format!(
                "registry temporary directory already exists '{}'",
                plan.temporary_dir.display()
            ),
            span,
        ));
    }
    fs::create_dir(&plan.temporary_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create registry temporary directory '{}': {err}",
            plan.temporary_dir.display()
        ))
    })?;
    let mut temporary = TemporaryDirectory::new(plan.temporary_dir.clone());
    let temporary_artifact = plan.temporary_dir.join(REGISTRY_ARTIFACT_FILE);
    download_https_to_file(
        &plan.url,
        &temporary_artifact,
        &plan.checksum,
        plan.policy,
        span,
    )?;
    let checksum_path = plan.temporary_dir.join(REGISTRY_CHECKSUM_FILE);
    let mut checksum_file = fs::File::create(&checksum_path).map_err(|err| {
        KuError::message(format!(
            "failed to write registry checksum metadata '{}': {err}",
            plan.temporary_dir.display()
        ))
    })?;
    writeln!(checksum_file, "{}", plan.checksum).map_err(|err| {
        KuError::message(format!(
            "failed to write registry checksum metadata '{}': {err}",
            checksum_path.display()
        ))
    })?;
    checksum_file.sync_all().map_err(|err| {
        KuError::message(format!(
            "failed to sync registry checksum metadata '{}': {err}",
            checksum_path.display()
        ))
    })?;

    let lock_path = parent.join(format!(
        "{}.install.lock",
        plan.target_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("package")
    ));
    let _lock = acquire_registry_install_lock(&lock_path, span)?;
    if artifact.is_file()
        && sha256_file_matches(&artifact, &plan.checksum, plan.policy.max_download_bytes)?
    {
        return Ok(artifact);
    }
    install_immutable_registry_cache(&plan.temporary_dir, &plan.target_dir, span)?;
    temporary.keep();
    Ok(artifact)
}

fn finish_registry_index_version(
    name: Option<&str>,
    fields: HashMap<String, String>,
    index_url: &str,
    span: Span,
) -> KuResult<RegistryManifest> {
    reject_unknown_fields(
        &fields,
        &["version", "url", "checksum"],
        "registry index version",
        span,
    )?;
    let name = name.ok_or_else(|| {
        KuError::package(
            "missing_registry_field",
            "registry index version appears before package name",
            span,
        )
    })?;
    let version = required_field(&fields, "version", "registry index version", span)?;
    let url = required_field(&fields, "url", "registry index version", span)?;
    let checksum = required_field(&fields, "checksum", "registry index version", span)?;
    validate_package_name(name, span)?;
    validate_version(&version, span)?;
    validate_sha256_checksum(&checksum, span)?;
    Ok(RegistryManifest {
        name: name.to_string(),
        version,
        source: resolve_registry_url(index_url, &url, span)?,
        checksum,
    })
}

pub fn resolve_registry_url(base_url: &str, value: &str, span: Span) -> KuResult<String> {
    validate_registry_url(base_url, span)?;
    if value.chars().any(char::is_whitespace) {
        return Err(invalid_registry_url_error(span));
    }
    let base = Url::parse(base_url).map_err(|_| invalid_registry_url_error(span))?;
    let resolved = base
        .join(value)
        .map_err(|_| invalid_registry_url_error(span))?;
    validate_parsed_registry_url(&resolved, span)?;
    Ok(resolved.into())
}

fn fetch_https_bytes(url: &str, policy: RegistryFetchPolicy, span: Span) -> KuResult<Vec<u8>> {
    retry_registry_request(policy, span, || {
        let response = registry_get(url, policy)?;
        let mut bytes = Vec::new();
        read_limited(
            response.into_reader(),
            &mut bytes,
            policy.max_download_bytes,
        )
        .map_err(classify_stream_error)?;
        Ok(bytes)
    })
}

fn download_https_to_file(
    url: &str,
    path: &Path,
    expected_checksum: &str,
    policy: RegistryFetchPolicy,
    span: Span,
) -> KuResult<()> {
    retry_registry_request(policy, span, || {
        let response = registry_get(url, policy)?;
        let mut file = fs::File::create(path).map_err(|err| {
            FetchAttemptError::Fatal(KuError::message(format!(
                "failed to create registry download '{}': {err}",
                path.display()
            )))
        })?;
        let actual = stream_sha256(response.into_reader(), &mut file, policy.max_download_bytes)
            .map_err(classify_stream_error)?;
        file.sync_all().map_err(|err| {
            FetchAttemptError::Fatal(KuError::message(format!(
                "failed to sync registry download '{}': {err}",
                path.display()
            )))
        })?;
        if !checksum_hex(expected_checksum).eq_ignore_ascii_case(&actual) {
            return Err(FetchAttemptError::Fatal(KuError::package(
                "checksum_mismatch",
                format!(
                    "registry package checksum mismatch: expected {expected_checksum}, got sha256-{actual}"
                ),
                span,
            )));
        }
        Ok(())
    })
}

enum FetchAttemptError {
    Retry(String),
    Fatal(KuError),
}

fn classify_stream_error(err: KuError) -> FetchAttemptError {
    if err.code.as_deref() == Some("registry_read_failed") {
        FetchAttemptError::Retry(err.to_string())
    } else {
        FetchAttemptError::Fatal(err)
    }
}

fn retry_registry_request<T>(
    policy: RegistryFetchPolicy,
    span: Span,
    mut request: impl FnMut() -> Result<T, FetchAttemptError>,
) -> KuResult<T> {
    let mut last_error = String::new();
    for attempt in 1..=policy.max_attempts {
        match request() {
            Ok(value) => return Ok(value),
            Err(FetchAttemptError::Fatal(err)) => return Err(err),
            Err(FetchAttemptError::Retry(message)) => {
                last_error = message;
                if attempt < policy.max_attempts {
                    let delay_ms = 25u64.saturating_mul(1u64 << (attempt - 1).min(7));
                    thread::sleep(Duration::from_millis(delay_ms));
                }
            }
        }
    }
    Err(KuError::package(
        "registry_fetch_failed",
        format!(
            "registry request failed after {} attempts: {last_error}",
            policy.max_attempts
        ),
        span,
    ))
}

fn registry_get(
    url: &str,
    policy: RegistryFetchPolicy,
) -> Result<ureq::Response, FetchAttemptError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(policy.connect_timeout_ms))
        .timeout(Duration::from_millis(policy.read_timeout_ms))
        .timeout_read(Duration::from_millis(policy.read_timeout_ms))
        .redirects(0)
        .build();
    match agent.get(url).set("Accept-Encoding", "identity").call() {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(status, _))
            if matches!(status, 408 | 429 | 500 | 502 | 503 | 504) =>
        {
            Err(FetchAttemptError::Retry(format!(
                "server returned retryable HTTP status {status}"
            )))
        }
        Err(ureq::Error::Status(status, _)) => Err(FetchAttemptError::Fatal(KuError::package(
            "registry_http_status",
            format!("registry server returned non-retryable HTTP status {status}"),
            Span::default(),
        ))),
        Err(ureq::Error::Transport(err)) => Err(FetchAttemptError::Retry(err.to_string())),
    }
}

fn read_limited(mut reader: impl Read, mut writer: impl Write, max_bytes: u64) -> KuResult<u64> {
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer).map_err(|err| {
            KuError::package(
                "registry_read_failed",
                format!("failed to read registry response: {err}"),
                Span::default(),
            )
        })?;
        if count == 0 {
            return Ok(total);
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes {
            return Err(KuError::package(
                "download_limit",
                format!("registry response exceeds {max_bytes} bytes"),
                Span::default(),
            ));
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|err| KuError::message(format!("failed to write registry response: {err}")))?;
    }
}

fn stream_sha256(
    mut reader: impl Read,
    mut writer: impl Write,
    max_bytes: u64,
) -> KuResult<String> {
    let mut total = 0u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer).map_err(|err| {
            KuError::package(
                "registry_read_failed",
                format!("failed to read registry response: {err}"),
                Span::default(),
            )
        })?;
        if count == 0 {
            return Ok(format!("{:x}", hasher.finalize()));
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes {
            return Err(KuError::package(
                "download_limit",
                format!("registry package exceeds {max_bytes} bytes"),
                Span::default(),
            ));
        }
        hasher.update(&buffer[..count]);
        writer
            .write_all(&buffer[..count])
            .map_err(|err| KuError::message(format!("failed to write registry package: {err}")))?;
    }
}

fn sha256_file_matches(path: &Path, checksum: &str, max_bytes: u64) -> KuResult<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to inspect registry cache '{}': {err}",
                path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes {
        return Ok(false);
    }
    let file = fs::File::open(path).map_err(|err| {
        KuError::message(format!(
            "failed to open registry cache '{}': {err}",
            path.display()
        ))
    })?;
    let actual = stream_sha256(file, io::sink(), max_bytes)?;
    Ok(checksum_hex(checksum).eq_ignore_ascii_case(&actual))
}

fn checksum_hex(checksum: &str) -> &str {
    checksum.strip_prefix("sha256-").unwrap_or("")
}

struct TemporaryDirectory {
    path: PathBuf,
    remove_on_drop: bool,
}

impl TemporaryDirectory {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            remove_on_drop: true,
        }
    }

    fn keep(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct RegistryInstallLock {
    path: PathBuf,
}

impl Drop for RegistryInstallLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_registry_install_lock(path: &Path, span: Span) -> KuResult<RegistryInstallLock> {
    for attempt in 1..=REGISTRY_INSTALL_LOCK_ATTEMPTS {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                if let Err(err) = writeln!(file, "{}", std::process::id()) {
                    drop(file);
                    let _ = fs::remove_file(path);
                    return Err(KuError::message(format!(
                        "failed to write registry install lock '{}': {err}",
                        path.display()
                    )));
                }
                return Ok(RegistryInstallLock {
                    path: path.to_path_buf(),
                });
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if registry_install_lock_is_stale(path) {
                    let _ = fs::remove_file(path);
                    continue;
                }
                if attempt < REGISTRY_INSTALL_LOCK_ATTEMPTS {
                    thread::sleep(Duration::from_millis(REGISTRY_INSTALL_LOCK_DELAY_MS));
                }
            }
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to create registry install lock '{}': {err}",
                    path.display()
                )));
            }
        }
    }
    Err(KuError::package(
        "registry_cache_busy",
        format!(
            "registry cache remained locked after {} bounded attempts",
            REGISTRY_INSTALL_LOCK_ATTEMPTS
        ),
        span,
    ))
}

fn registry_install_lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
        .is_ok_and(|age| age >= Duration::from_secs(REGISTRY_INSTALL_LOCK_STALE_SECS))
}

fn install_immutable_registry_cache(source: &Path, target: &Path, span: Span) -> KuResult<()> {
    if target.exists() {
        return Err(KuError::package(
            "registry_cache_conflict",
            format!(
                "content-addressed registry cache already exists but did not verify '{}'",
                target.display()
            ),
            span,
        ));
    }
    fs::rename(source, target).map_err(|err| {
        KuError::message(format!(
            "failed to atomically install registry cache '{}': {err}",
            target.display()
        ))
    })
}

fn registry_cache_key(manifest: &RegistryManifest) -> String {
    let digest = manifest
        .checksum
        .strip_prefix("sha256-")
        .unwrap_or(&manifest.checksum);
    format!("{}-{}-sha256-{digest}", manifest.name, manifest.version)
}

pub fn parse_registry_lock(source: &str, span: Span) -> KuResult<Vec<RegistryLockPackage>> {
    let mut packages = Vec::new();
    let mut current = None::<HashMap<String, String>>;
    for (index, raw_line) in source.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[package]]" {
            if let Some(fields) = current.take() {
                packages.push(finish_registry_lock_package(fields, span)?);
            }
            current = Some(HashMap::new());
            continue;
        }
        let Some(fields) = current.as_mut() else {
            return Err(KuError::package(
                "invalid_registry_lock",
                format!(
                    "invalid registry lock line {}: expected [[package]]",
                    index + 1
                ),
                span,
            ));
        };
        let Some((key, raw_value)) = line.split_once('=') else {
            return Err(KuError::package(
                "invalid_registry_lock",
                format!(
                    "invalid registry lock line {}: expected key = value",
                    index + 1
                ),
                span,
            ));
        };
        let key = key.trim().to_string();
        let value = parse_string_value(raw_value.trim(), index + 1, span)?;
        if fields.insert(key.clone(), value).is_some() {
            return Err(KuError::package(
                "duplicate_registry_lock_field",
                format!("duplicate registry lock field '{key}'"),
                span,
            ));
        }
    }
    if let Some(fields) = current {
        packages.push(finish_registry_lock_package(fields, span)?);
    }
    if packages.is_empty() {
        return Err(KuError::package(
            "empty_registry_lock",
            "registry lock must contain at least one [[package]] entry",
            span,
        ));
    }
    Ok(packages)
}

fn finish_registry_lock_package(
    fields: HashMap<String, String>,
    span: Span,
) -> KuResult<RegistryLockPackage> {
    reject_unknown_fields(
        &fields,
        &["name", "version", "source", "url", "checksum", "cache_key"],
        "registry lock package",
        span,
    )?;
    let name = required_field(&fields, "name", "registry lock package", span)?;
    let version = required_field(&fields, "version", "registry lock package", span)?;
    let source = required_field(&fields, "source", "registry lock package", span)?;
    let url = required_field(&fields, "url", "registry lock package", span)?;
    let checksum = required_field(&fields, "checksum", "registry lock package", span)?;
    let cache_key = required_field(&fields, "cache_key", "registry lock package", span)?;
    validate_package_name(&name, span)?;
    validate_version(&version, span)?;
    if source != "registry" {
        return Err(KuError::package(
            "invalid_registry_source",
            "registry lock source must be \"registry\"",
            span,
        ));
    }
    validate_registry_url(&url, span)?;
    validate_sha256_checksum(&checksum, span)?;
    let expected_cache_key = registry_cache_key(&RegistryManifest {
        name: name.clone(),
        version: version.clone(),
        source: url.clone(),
        checksum: checksum.clone(),
    });
    if cache_key != expected_cache_key {
        return Err(KuError::package(
            "invalid_cache_key",
            format!(
                "registry lock cache_key must be derived from name, version, and checksum; expected '{expected_cache_key}'"
            ),
            span,
        ));
    }
    Ok(RegistryLockPackage {
        name,
        version,
        source,
        url,
        checksum,
        cache_key,
    })
}

fn parse_flat_string_fields(
    source: &str,
    kind: &str,
    span: Span,
) -> KuResult<HashMap<String, String>> {
    let mut fields = HashMap::new();
    for (index, raw_line) in source.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            return Err(KuError::package(
                "invalid_registry_manifest",
                format!("invalid {kind} line {}: expected key = value", index + 1),
                span,
            ));
        };
        let key = key.trim().to_string();
        let value = parse_string_value(raw_value.trim(), index + 1, span)?;
        if fields.insert(key.clone(), value).is_some() {
            return Err(KuError::package(
                "duplicate_registry_field",
                format!("duplicate {kind} field '{key}'"),
                span,
            ));
        }
    }
    Ok(fields)
}

fn required_field(
    fields: &HashMap<String, String>,
    name: &str,
    kind: &str,
    span: Span,
) -> KuResult<String> {
    fields.get(name).cloned().ok_or_else(|| {
        KuError::package(
            "missing_registry_field",
            format!("{kind} missing required field '{name}'"),
            span,
        )
    })
}

fn reject_unknown_fields(
    fields: &HashMap<String, String>,
    allowed: &[&str],
    kind: &str,
    span: Span,
) -> KuResult<()> {
    if let Some(name) = fields.keys().find(|name| !allowed.contains(&name.as_str())) {
        return Err(KuError::package(
            "invalid_registry_field",
            format!("{kind} has unsupported field '{name}'"),
            span,
        ));
    }
    Ok(())
}

#[derive(Default)]
struct PackageDependencyDraft {
    name: String,
    version: Option<String>,
    source: Option<String>,
    checksum: Option<String>,
}

impl PackageDependencyDraft {
    fn finish(self, span: Span) -> KuResult<PackageDependency> {
        validate_package_name(&self.name, span)?;
        let version = self.version.ok_or_else(|| {
            KuError::package(
                "missing_dependency_version",
                format!("dependency '{}' missing version", self.name),
                span,
            )
        })?;
        validate_version_requirement(&version, span)?;
        if let Some(checksum) = &self.checksum {
            validate_checksum(checksum, span)?;
        }
        Ok(PackageDependency {
            name: self.name,
            version,
            source: self.source,
            checksum: self.checksum,
        })
    }
}

impl PackageDependency {
    fn root(&self) -> &'static str {
        DEFAULT_IMPORT_ROOT
    }
}

fn parse_dependency_key(
    key: &str,
    value: String,
    dependencies: &mut HashMap<String, PackageDependencyDraft>,
    line: usize,
    span: Span,
) -> KuResult<()> {
    let rest = key.trim_start_matches("dep.");
    let (name, field) = rest
        .split_once('.')
        .map(|(name, field)| (name, Some(field)))
        .unwrap_or((rest, None));
    if name.is_empty() {
        return Err(KuError::package(
            "invalid_dependency_key",
            format!("invalid dependency key on line {line}"),
            span,
        ));
    }
    let dependency =
        dependencies
            .entry(name.to_string())
            .or_insert_with(|| PackageDependencyDraft {
                name: name.to_string(),
                ..Default::default()
            });
    match field {
        None => dependency.version = Some(value),
        Some("source") => dependency.source = Some(value),
        Some("checksum") => dependency.checksum = Some(value),
        Some(other) => {
            return Err(KuError::package(
                "invalid_dependency_field",
                format!("invalid dependency field '{other}' on line {line}"),
                span,
            ));
        }
    }
    Ok(())
}

fn parse_string_value(value: &str, line: usize, span: Span) -> KuResult<String> {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        Ok(value[1..value.len() - 1].to_string())
    } else {
        Err(KuError::package(
            "invalid_manifest_value",
            format!("invalid ku.mod value on line {line}: expected quoted string"),
            span,
        ))
    }
}

fn validate_package_name(name: &str, span: Span) -> KuResult<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(KuError::package(
            "invalid_name",
            "package name cannot be empty",
            span,
        ));
    };
    if !first.is_ascii_lowercase() {
        return Err(KuError::package(
            "invalid_name",
            "package name must start with a lowercase ascii letter",
            span,
        ));
    }
    if !chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-') {
        return Err(KuError::package(
            "invalid_name",
            "package name may only contain lowercase letters, digits, '_' and '-'",
            span,
        ));
    }
    Ok(())
}

fn validate_manifest_label(kind: &str, value: &str, span: Span) -> KuResult<()> {
    if value.is_empty() {
        return Err(KuError::package(
            "invalid_manifest_label",
            format!("ku.mod {kind} cannot be empty"),
            span,
        ));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
    {
        return Err(KuError::package(
            "invalid_manifest_label",
            format!("ku.mod {kind} may only contain lowercase letters, digits, '_' and '-'"),
            span,
        ));
    }
    Ok(())
}

fn validate_version(version: &str, span: Span) -> KuResult<()> {
    parse_package_version(version, span).map(|_| ())
}

fn validate_version_requirement(version: &str, span: Span) -> KuResult<()> {
    parse_version_requirement(version, span).map(|_| ())
}

fn invalid_version_error(span: Span) -> KuError {
    KuError::package(
        "invalid_version",
        "package version must use major.minor.patch digits within u64 range",
        span,
    )
}

fn invalid_version_requirement_error(span: Span) -> KuError {
    KuError::package(
        "invalid_version_requirement",
        "dependency version must be an exact major.minor.patch version or a ^major.minor.patch range",
        span,
    )
}

fn validate_registry_fetch_policy(policy: RegistryFetchPolicy, span: Span) -> KuResult<()> {
    if policy.max_attempts == 0 || policy.max_attempts > MAX_REGISTRY_FETCH_ATTEMPTS {
        return Err(KuError::package(
            "invalid_fetch_policy",
            format!("registry max_attempts must be between 1 and {MAX_REGISTRY_FETCH_ATTEMPTS}"),
            span,
        ));
    }
    if policy.connect_timeout_ms == 0
        || policy.connect_timeout_ms > MAX_REGISTRY_TIMEOUT_MS
        || policy.read_timeout_ms == 0
        || policy.read_timeout_ms > MAX_REGISTRY_TIMEOUT_MS
    {
        return Err(KuError::package(
            "invalid_fetch_policy",
            format!(
                "registry timeouts must be between 1 and {MAX_REGISTRY_TIMEOUT_MS} milliseconds"
            ),
            span,
        ));
    }
    if policy.max_download_bytes == 0 || policy.max_download_bytes > MAX_REGISTRY_DOWNLOAD_BYTES {
        return Err(KuError::package(
            "invalid_fetch_policy",
            format!(
                "registry max_download_bytes must be between 1 and {MAX_REGISTRY_DOWNLOAD_BYTES}"
            ),
            span,
        ));
    }
    Ok(())
}

fn reject_unsafe_relative_path(kind: &str, value: &str, span: Span) -> KuResult<()> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(KuError::package(
            "unsafe_path",
            format!("ku.mod {kind} must be a safe relative path"),
            span,
        ));
    }
    Ok(())
}

fn reject_unsafe_dependency_import(value: &str, span: Span) -> KuResult<()> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(KuError::package(
            "unsafe_import",
            "package dependency import must stay inside dependency root",
            span,
        ));
    }
    Ok(())
}

fn validate_checksum(value: &str, span: Span) -> KuResult<()> {
    let Some(hex) = value.strip_prefix("ku-fnv64-") else {
        return Err(KuError::package(
            "invalid_checksum",
            "dependency checksum must use ku-fnv64- followed by 16 hex digits",
            span,
        ));
    };
    if hex.len() != 16 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(KuError::package(
            "invalid_checksum",
            "dependency checksum must use ku-fnv64- followed by 16 hex digits",
            span,
        ));
    }
    Ok(())
}

fn validate_sha256_checksum(value: &str, span: Span) -> KuResult<()> {
    let Some(hex) = value.strip_prefix("sha256-") else {
        return Err(KuError::package(
            "invalid_registry_checksum",
            "registry checksum must use sha256- followed by 64 hex digits",
            span,
        ));
    };
    if hex.len() != 64 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(KuError::package(
            "invalid_registry_checksum",
            "registry checksum must use sha256- followed by 64 hex digits",
            span,
        ));
    }
    Ok(())
}

fn validate_registry_url(value: &str, span: Span) -> KuResult<()> {
    if value.chars().any(char::is_whitespace) {
        return Err(invalid_registry_url_error(span));
    }
    let parsed = Url::parse(value).map_err(|_| invalid_registry_url_error(span))?;
    validate_parsed_registry_url(&parsed, span)
}

fn validate_parsed_registry_url(value: &Url, span: Span) -> KuResult<()> {
    if value.scheme() != "https"
        || value.host_str().is_none()
        || !value.username().is_empty()
        || value.password().is_some()
        || value.fragment().is_some()
        || value.as_str().chars().any(char::is_whitespace)
    {
        return Err(invalid_registry_url_error(span));
    }
    Ok(())
}

fn invalid_registry_url_error(span: Span) -> KuError {
    KuError::package(
        "invalid_registry_url",
        "registry URL must use HTTPS, include a host, and omit credentials, fragments, and whitespace",
        span,
    )
}

fn dependency_cache_root(package: &PackageContext, dependency: &PackageDependency) -> PathBuf {
    package
        .cache_dir
        .join(PACKAGE_CACHE_DIR)
        .join(&dependency.name)
        .join(&dependency.version)
}

fn file_url_path(source: &str) -> Option<PathBuf> {
    let raw = source.strip_prefix("file://")?;
    let path = if raw.starts_with('/') && raw.as_bytes().get(2) == Some(&b':') {
        &raw[1..]
    } else {
        raw
    };
    percent_decode(path).map(PathBuf::from)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = *bytes.get(index + 1)?;
            let lo = *bytes.get(index + 2)?;
            output.push((hex_value(hi)? << 4) | hex_value(lo)?);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn copy_package_source(source: &Path, target: &Path) -> KuResult<()> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    copy_package_source_inner(source, source, target, &mut files, &mut bytes)
}

fn copy_package_source_inner(
    root: &Path,
    current: &Path,
    target_root: &Path,
    files: &mut usize,
    bytes: &mut u64,
) -> KuResult<()> {
    for entry in fs::read_dir(current).map_err(|err| {
        KuError::message(format!(
            "failed to read package source '{}': {err}",
            current.display()
        ))
    })? {
        let entry = entry
            .map_err(|err| KuError::message(format!("failed to read package entry: {err}")))?;
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|err| {
            KuError::message(format!(
                "failed to compute package relative path '{}': {err}",
                path.display()
            ))
        })?;
        let target = target_root.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            KuError::message(format!(
                "failed to read package metadata '{}': {err}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(KuError::package(
                "unsupported_symlink",
                format!(
                    "package source contains unsupported symlink '{}'",
                    path.display()
                ),
                Span::default(),
            ));
        }
        if metadata.is_dir() {
            fs::create_dir_all(&target).map_err(|err| {
                KuError::message(format!(
                    "failed to create package cache directory '{}': {err}",
                    target.display()
                ))
            })?;
            copy_package_source_inner(root, &path, target_root, files, bytes)?;
        } else if metadata.is_file() {
            *files += 1;
            *bytes += metadata.len();
            if *files > MAX_PACKAGE_FILES || *bytes > MAX_PACKAGE_BYTES {
                return Err(KuError::package(
                    "cache_limit",
                    "package source exceeds cache limits",
                    Span::default(),
                ));
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    KuError::message(format!(
                        "failed to create package cache directory '{}': {err}",
                        parent.display()
                    ))
                })?;
            }
            fs::copy(&path, &target).map_err(|err| {
                KuError::message(format!(
                    "failed to copy package file '{}' to '{}': {err}",
                    path.display(),
                    target.display()
                ))
            })?;
        }
    }
    Ok(())
}

fn collect_source_bytes(
    root: &Path,
    current: &Path,
    output: &mut Vec<u8>,
    files: &mut usize,
    bytes: &mut u64,
) -> KuResult<()> {
    let mut entries = fs::read_dir(current)
        .map_err(|err| {
            KuError::message(format!(
                "failed to read package source '{}': {err}",
                current.display()
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| KuError::message(format!("failed to read package source entry: {err}")))?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            KuError::message(format!(
                "failed to read package metadata '{}': {err}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(KuError::package(
                "unsupported_symlink",
                format!(
                    "package source contains unsupported symlink '{}'",
                    path.display()
                ),
                Span::default(),
            ));
        }
        if metadata.is_dir() {
            collect_source_bytes(root, &path, output, files, bytes)?;
        } else if metadata.is_file() {
            *files += 1;
            *bytes += metadata.len();
            if *files > MAX_PACKAGE_FILES || *bytes > MAX_PACKAGE_BYTES {
                return Err(KuError::package(
                    "checksum_limit",
                    "package source exceeds checksum limits",
                    Span::default(),
                ));
            }
            let relative = path.strip_prefix(root).map_err(|err| {
                KuError::message(format!(
                    "failed to compute package relative path '{}': {err}",
                    path.display()
                ))
            })?;
            output.extend_from_slice(relative.to_string_lossy().replace('\\', "/").as_bytes());
            output.push(0);
            output.extend(fs::read(&path).map_err(|err| {
                KuError::message(format!(
                    "failed to read package file '{}': {err}",
                    path.display()
                ))
            })?);
            output.push(0);
        }
    }
    Ok(())
}

pub(crate) fn ensure_inside_import_root(
    path: &Path,
    package: &PackageContext,
    span: Span,
) -> KuResult<()> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if canonical.starts_with(&package.import_root) {
        Ok(())
    } else {
        Err(KuError::runtime(
            format!(
                "import '{}' is outside package import root '{}'",
                path.display(),
                package.import_root.display()
            ),
            span,
        ))
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn temp_path(label: &str) -> PathBuf {
        let id = NEXT_REGISTRY_DOWNLOAD_ID.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!("ku-{label}-{}-{id}", std::process::id()))
    }

    #[test]
    fn static_index_resolves_https_urls_and_sorts_versions() {
        let index = parse_registry_index(
            r#"
name = "math"

[[version]]
version = "1.2.3"
url = "../packages/math-1.2.3.tar.gz"
checksum = "sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[[version]]
version = "2.0.0"
url = "https://cdn.example/math-2.0.0.tar.gz"
checksum = "sha256-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#,
            "https://registry.example/index/math",
            Span::default(),
        )
        .expect("valid static index");

        assert_eq!(index.name, "math");
        assert_eq!(index.versions[0].version, "2.0.0");
        assert_eq!(
            index.versions[1].source,
            "https://registry.example/packages/math-1.2.3.tar.gz"
        );
    }

    #[test]
    fn registry_rejects_http_and_duplicate_index_versions() {
        let manifest = RegistryManifest {
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            source: "http://registry.example/math.tar.gz".to_string(),
            checksum: "sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
        };
        let err = plan_registry_download(
            Path::new("cache"),
            &manifest,
            None,
            RegistryFetchPolicy::default(),
            Span::default(),
        )
        .expect_err("HTTP must be rejected");
        assert_eq!(err.code.as_deref(), Some("invalid_registry_url"));

        let err = parse_registry_index(
            r#"
name = "math"
[[version]]
version = "1.0.0"
url = "math.tar.gz"
checksum = "sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[[version]]
version = "1.0.0"
url = "math-copy.tar.gz"
checksum = "sha256-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#,
            "https://registry.example/index/math",
            Span::default(),
        )
        .expect_err("duplicate versions must be rejected");
        assert_eq!(err.code.as_deref(), Some("duplicate_registry_version"));
    }

    #[test]
    fn sha256_streaming_enforces_the_download_limit() {
        let mut output = Vec::new();
        let digest = stream_sha256(&b"abc"[..], &mut output, 3).expect("bounded stream");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(output, b"abc");

        let err =
            stream_sha256(&b"abcd"[..], io::sink(), 3).expect_err("oversized stream must fail");
        assert_eq!(err.code.as_deref(), Some("download_limit"));
    }

    #[test]
    fn retry_loop_stops_at_the_configured_attempt_count() {
        let attempts = AtomicUsize::new(0);
        let policy = RegistryFetchPolicy {
            max_attempts: 3,
            ..RegistryFetchPolicy::default()
        };
        let err = retry_registry_request(policy, Span::default(), || {
            attempts.fetch_add(1, AtomicOrdering::Relaxed);
            Err::<(), _>(FetchAttemptError::Retry("temporary".to_string()))
        })
        .expect_err("bounded retries must eventually fail");
        assert_eq!(attempts.load(AtomicOrdering::Relaxed), 3);
        assert_eq!(err.code.as_deref(), Some("registry_fetch_failed"));
    }

    #[test]
    fn fatal_fetch_errors_are_not_retried() {
        let attempts = AtomicUsize::new(0);
        let err = retry_registry_request(RegistryFetchPolicy::default(), Span::default(), || {
            attempts.fetch_add(1, AtomicOrdering::Relaxed);
            Err::<(), _>(FetchAttemptError::Fatal(KuError::package(
                "checksum_mismatch",
                "bad checksum",
                Span::default(),
            )))
        })
        .expect_err("fatal errors must stop immediately");
        assert_eq!(attempts.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(err.code.as_deref(), Some("checksum_mismatch"));
    }

    #[test]
    fn immutable_cache_install_refuses_to_replace_existing_content() {
        let root = temp_path("registry-cache-install");
        let target = root.join("math-1.0.0");
        let source = root.join("math-1.0.0.download");
        fs::create_dir_all(&target).expect("create old cache");
        fs::create_dir_all(&source).expect("create new cache");
        fs::write(target.join("old"), b"old").expect("write old cache");
        fs::write(source.join(REGISTRY_ARTIFACT_FILE), b"new").expect("write new cache");

        let err = install_immutable_registry_cache(&source, &target, Span::default())
            .expect_err("content-addressed cache must never replace an existing directory");
        assert_eq!(err.code.as_deref(), Some("registry_cache_conflict"));
        assert!(source.exists());
        assert_eq!(
            fs::read(target.join("old")).expect("read old cache"),
            b"old"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn verified_cache_is_reused_without_network_access() {
        let root = temp_path("registry-cache-reuse");
        let target = root.join("packages").join("math").join("1.0.0");
        fs::create_dir_all(&target).expect("create cache");
        fs::write(target.join(REGISTRY_ARTIFACT_FILE), b"abc").expect("write artifact");
        let plan = RegistryDownloadPlan {
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            url: "https://unreachable.invalid/math.tar.gz".to_string(),
            checksum: "sha256-ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                .to_string(),
            target_dir: target.clone(),
            temporary_dir: target.with_extension("download-test"),
            action: RegistryCacheAction::ReuseVerified,
            policy: RegistryFetchPolicy::default(),
        };

        let artifact =
            execute_registry_download(&plan, Span::default()).expect("verified cache reuse");
        assert_eq!(artifact, target.join(REGISTRY_ARTIFACT_FILE));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn trust_boundary_refuses_unconfigured_signature_verification() {
        let err = reject_unconfigured_registry_index_trust(Span::default());
        assert_eq!(err.code.as_deref(), Some("registry_trust_unconfigured"));
    }
}
