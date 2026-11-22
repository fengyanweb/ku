use super::*;
use crate::{ast::Item, lexer::Lexer, parser::Parser};
use std::process::{Command, Stdio};

const MAX_GIT_ARCHIVE_BYTES: usize = MAX_PACKAGE_BYTES as usize + 2 * 1024 * 1024;
const INSTALL_TRANSACTION: &str = "ku.lock.install-transaction";
const TRANSACTION_MAGIC: &[u8] = b"KU-INSTALL-1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInstallReceipt {
    pub name: String,
    pub version: String,
    pub source: String,
    pub package_root: PathBuf,
    pub imports: Vec<PackageInstallModule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInstallModule {
    pub path: String,
    pub exports: Vec<String>,
}

/// Adds one direct dependency or restores the existing selection.
pub fn install_dependency(
    project: &mut PackageContext,
    request: Option<&str>,
    development: bool,
    production: bool,
    mode: DependencyResolveMode,
) -> KuResult<Vec<PackageInstallReceipt>> {
    install_dependency_with(project, request, development, production, mode, |_, _| {
        Ok(())
    })
}

pub fn install_dependency_with<F>(
    project: &mut PackageContext,
    request: Option<&str>,
    development: bool,
    production: bool,
    mode: DependencyResolveMode,
    before_commit: F,
) -> KuResult<Vec<PackageInstallReceipt>>
where
    F: FnOnce(&PackageContext, &[PackageInstallReceipt]) -> KuResult<()>,
{
    let strict = matches!(
        mode,
        DependencyResolveMode::Locked | DependencyResolveMode::Offline
    );
    if (request.is_some() && (strict || production)) || (request.is_none() && development) {
        return Err(error("invalid_install_options", "adding a dependency cannot be combined with --prod, --locked, or --offline; --dev requires a dependency"));
    }
    if mode == DependencyResolveMode::Refresh {
        return Err(error(
            "invalid_install_options",
            "use ku package resolve to refresh locked dependencies",
        ));
    }

    let deadline = package_operation_deadline();
    let install_lock = project.package_dir.join(".ku/install.io.lock");
    let _installer = acquire_registry_install_lock_until(&install_lock, Span::default(), deadline)?;
    recover_transaction_if_needed(&project.manifest_path)?;
    let original = read_manifest_file(&project.manifest_path)?;
    let mut candidate = project.clone();
    candidate.manifest = parse_manifest(&original, Span::default())?;
    if candidate.manifest.cache != project.manifest.cache {
        return Err(error(
            "manifest_changed",
            "ku.mod cache path changed while installing; retry with the new project configuration",
        ));
    }
    let _usage = acquire_package_usage_lease_until(&candidate, deadline)?;
    let prior_lock = read_optional_regular(&candidate.lock_path, MAX_PACKAGE_LOCK_BYTES)?;
    if strict {
        validate_lock_fingerprint(&candidate, prior_lock.as_deref(), production)?;
    }

    let (manifest_source, requested_name) = if let Some(request) = request {
        let dependency = identify_request(&candidate, request, deadline)?;
        let name = dependency.name.clone();
        let source = declare_dependency(&candidate.manifest, &original, &dependency, development)?;
        candidate.manifest = parse_manifest(&source, Span::default())?;
        (source, Some(name))
    } else {
        (original.clone(), None)
    };

    let can_restore = request.is_none()
        && mode == DependencyResolveMode::Update
        && prior_lock.as_deref().is_some_and(|lock| {
            validate_lock_fingerprint(&candidate, Some(lock), production).is_ok()
        });
    let restored_existing = if can_restore {
        match resolve_scoped_dependencies_with_mode_until(
            &mut candidate,
            DependencyResolveMode::Locked,
            deadline,
            production,
        ) {
            Ok(()) => true,
            Err(err) if err.code.as_deref() == Some("lock_out_of_date") => false,
            Err(err) => return Err(err),
        }
    } else {
        false
    };
    if !restored_existing {
        resolve_scoped_dependencies_with_mode_until(&mut candidate, mode, deadline, production)?;
    }
    let names = candidate
        .manifest
        .direct_dependencies(production)
        .filter(|dep| requested_name.as_ref().is_none_or(|name| &dep.name == name))
        .map(|dep| dep.name.as_str())
        .collect::<HashSet<_>>();
    let receipts = install_receipts(&candidate, &names, deadline)?;
    before_commit(&candidate, &receipts)?;

    if !strict {
        // Retain a verified full lock after a production-only restore and preserve
        // the import snapshots after a warm installation of the same selection.
        let retain_original = restored_existing
            || (manifest_source == original
                && prior_lock.as_deref().is_some_and(|lock| {
                    validate_lock_fingerprint(&candidate, Some(lock), false).is_ok()
                        && lock_contains_graph(&candidate).unwrap_or(false)
                }));
        if !retain_original {
            let mut lock = package_lock_content(&candidate, &[])?;
            if production {
                lock.insert_str(0, "scope = \"production\"\n");
            }
            commit_install(
                &candidate,
                &original,
                prior_lock.as_deref(),
                &manifest_source,
                lock.as_bytes(),
                deadline,
            )?;
        }
    }
    *project = candidate;
    Ok(receipts)
}

fn error(code: &'static str, message: impl Into<String>) -> KuError {
    KuError::package(code, message.into(), Span::default())
}

fn lock_contains_graph(candidate: &PackageContext) -> KuResult<bool> {
    let files = read_locked_file_dependencies(candidate)?;
    let registry = read_locked_registry_dependencies(candidate)?;
    Ok(files.len() == candidate.resolved_file_dependencies.len()
        && registry.len() == candidate.resolved_registry_dependencies.len()
        && candidate.resolved_file_dependencies.iter().all(|dep| {
            files.get(&dep.name).is_some_and(|prior| {
                dep.source == prior.source
                    && dep.requirement == prior.requirement
                    && dep.checksum == prior.checksum
                    && dep.version == prior.version
            })
        })
        && candidate.resolved_registry_dependencies.iter().all(|dep| {
            registry.get(&dep.name).is_some_and(|prior| {
                dep.version == prior.version
                    && dep.url == prior.url
                    && dep.checksum == prior.checksum
                    && dep.registry == prior.registry
            })
        }))
}

pub(super) fn without_comment(line: &str) -> &str {
    let mut quoted = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '#' if !quoted => return line[..index].trim(),
            _ => {}
        }
    }
    line.trim()
}

pub(super) fn manifest_fingerprint(manifest: &KuMod) -> String {
    // The parser normalizes ordering and validates all fields. Ignore formatting
    // and comments while pinning dependency scopes, requirements and trust roots.
    let mut hash = Sha256::new_with_prefix(b"ku-package-install-manifest-v1\0");
    let mut field = |value: &str| {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    };
    field(&manifest.name);
    field(manifest.interop_version.as_deref().unwrap_or_default());
    field(manifest.version.as_deref().unwrap_or("0.0.0"));
    field(manifest.root.as_deref().unwrap_or(DEFAULT_IMPORT_ROOT));
    field(manifest.main.as_deref().unwrap_or(DEFAULT_MAIN_FILE));
    field(manifest.out.as_deref().unwrap_or(DEFAULT_BUILD_DIR));
    field(manifest.cache.as_deref().unwrap_or(DEFAULT_CACHE_DIR));
    if let Some(registry) = &manifest.registry {
        field(&registry.url);
        field(&registry.public_key);
    } else {
        field("");
        field("");
    }
    for (scope, group) in [
        ("dep", &manifest.dependencies),
        ("dev", &manifest.development_dependencies),
    ] {
        field(scope);
        field(&group.len().to_string());
        for dependency in group {
            field(&dependency.name);
            field(&dependency.version);
            field(dependency.source.as_deref().unwrap_or_default());
            field(dependency.checksum.as_deref().unwrap_or_default());
        }
    }
    format!("sha256-{:x}", hash.finalize())
}

fn validate_lock_fingerprint(
    project: &PackageContext,
    lock: Option<&[u8]>,
    production: bool,
) -> KuResult<()> {
    let lock = lock.ok_or_else(|| {
        error(
            "lock_out_of_date",
            "ku.lock is missing; run ku i to generate it",
        )
    })?;
    let lock = std::str::from_utf8(lock)
        .map_err(|_| error("invalid_package_lock", "ku.lock is not valid UTF-8"))?;
    let mut fingerprint = None;
    let mut scope = None;
    for (index, line) in lock
        .lines()
        .map(without_comment)
        .take_while(|line| !line.starts_with("[["))
        .enumerate()
    {
        if let Some((name, value)) = line.split_once('=') {
            let target = match name.trim() {
                "manifest_sha256" => &mut fingerprint,
                "scope" => &mut scope,
                _ => continue,
            };
            if target
                .replace(parse_string_value(
                    value.trim(),
                    index + 1,
                    Span::default(),
                )?)
                .is_some()
            {
                return Err(error(
                    "invalid_package_lock",
                    "duplicate install metadata in ku.lock",
                ));
            }
        }
    }
    if scope.as_deref().is_some_and(|name| name != "production") {
        return Err(error(
            "invalid_package_lock",
            "ku.lock contains an unknown restore scope",
        ));
    }
    if !production
        && scope.as_deref() == Some("production")
        && !project.manifest.development_dependencies.is_empty()
    {
        return Err(error(
            "lock_out_of_date",
            "ku.lock covers production only; run ku i to lock development dependencies",
        ));
    }
    if let Some(fingerprint) = fingerprint {
        if fingerprint != manifest_fingerprint(&project.manifest) {
            return Err(error(
                "lock_out_of_date",
                "ku.mod and ku.lock disagree; run ku i to refresh the affected dependencies",
            ));
        }
    } else if !project.manifest.development_dependencies.is_empty()
        || project.manifest.dependencies.iter().any(|dep| {
            dep.source
                .as_deref()
                .is_some_and(|source| !source.starts_with("file://"))
        })
    {
        return Err(error(
            "lock_out_of_date",
            "ku.lock predates the declared dependency sources; run ku i",
        ));
    }
    Ok(())
}

fn identify_request(
    project: &PackageContext,
    request: &str,
    deadline: Instant,
) -> KuResult<PackageDependency> {
    if request.starts_with("https://") || is_git_source(request) {
        let git = GitSource::parse(request, false, Span::default())?;
        if let Some(existing) = project.manifest.direct_dependencies(false).find(|dep| {
            dep.source
                .as_deref()
                .and_then(|source| GitSource::parse(source, true, Span::default()).ok())
                .is_some_and(|old| {
                    old.url == git.url
                        && old.subdirectory == git.subdirectory
                        && (git.revision == "HEAD"
                            || git.revision.eq_ignore_ascii_case(&old.revision))
                })
        }) {
            return Ok(existing.clone());
        }
        let _work = acquire_file_work_slot(&project.cache_dir, Span::default(), deadline)?;
        let (work, root, commit) = fetch_git_package(project, &git, deadline)?;
        let source = git.pin(&commit);
        let manifest = require_package(&root)?;
        let snapshot = snapshot_for_source(&root, &source, None)?;
        let dependency = PackageDependency {
            name: manifest.name,
            version: manifest.version.expect("required package version"),
            source: Some(source.clone()),
            checksum: Some(snapshot.checksum.clone()),
        };
        let identity = inspect_file_dependency_identity(&root, &dependency, Span::default())?;
        install_file_dependency(
            project,
            &dependency,
            &source,
            &root,
            identity,
            snapshot,
            deadline,
        )?;
        drop(work);
        return Ok(dependency);
    }

    let path = Path::new(request);
    if path.is_absolute()
        || request.starts_with("./")
        || request.starts_with(".\\")
        || request.starts_with("../")
        || request.starts_with("..\\")
        || path.components().count() > 1
    {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            project.package_dir.join(path)
        };
        let path = fs::canonicalize(&path).map_err(|err| {
            error(
                "local_package_unavailable",
                format!("cannot open local dependency '{}': {err}", path.display()),
            )
        })?;
        let relative = path.strip_prefix(&project.package_dir).map_err(|_| {
            error(
                "unsafe_local_dependency",
                "relative package dependencies must remain inside the project root",
            )
        })?;
        let relative = portable_path(relative)?;
        let manifest = require_package(&path)?;
        let source = format!("path:{relative}");
        validate_source(&source, Span::default())?;
        validate_file_dependency_source_is_external(project, &path)?;
        return Ok(PackageDependency {
            name: manifest.name,
            version: manifest.version.expect("required package version"),
            source: Some(source),
            checksum: None,
        });
    }

    validate_package_name(request, Span::default())?;
    if let Some(existing) = project
        .manifest
        .direct_dependencies(false)
        .find(|dep| dep.name == request)
    {
        if existing.source.is_some() {
            return Err(error("dependency_source_conflict", format!("'{request}' is already declared with another source; update its ku.mod declaration explicitly")));
        }
        return Ok(existing.clone());
    }
    let registry = project.manifest.registry.as_ref().ok_or_else(|| error(
        "registry_trust_unconfigured",
        "configure registry.url = \"https://YOUR-REGISTRY/v1/\" and registry.public_key = \"ed25519-<64 hex digits>\" in ku.mod, or use ku i ./vendor/package or ku i https://HOST/OWNER/REPO.git",
    ))?;
    let policy =
        registry_fetch_policy_for_deadline(deadline, MAX_REGISTRY_DOWNLOAD_BYTES, Span::default())?;
    let client = RegistryHttpClient::new(policy, &registry.url);
    let index =
        fetch_trusted_registry_index_until(registry, request, Span::default(), deadline, &client)?;
    let latest = index.versions.first().ok_or_else(|| {
        error(
            "registry_package_unavailable",
            format!("the configured registry has no installable version of '{request}'"),
        )
    })?;
    Ok(PackageDependency {
        name: request.to_owned(),
        version: format!("^{}", latest.version),
        source: None,
        checksum: None,
    })
}

fn require_package(root: &Path) -> KuResult<KuMod> {
    let manifest_path = root.join(MANIFEST_FILE);
    if !manifest_path.is_file() {
        return Err(error(
            "missing_package_manifest",
            format!(
                "'{}' has no ku.mod; supply a Ku package directory with ku.mod and src/",
                root.display()
            ),
        ));
    }
    let manifest = parse_manifest(&read_manifest_file(&manifest_path)?, Span::default())?;
    if manifest.version.is_none() {
        return Err(error(
            "missing_package_version",
            "installable Ku packages must declare version in ku.mod",
        ));
    }
    Ok(manifest)
}

fn declare_dependency(
    manifest: &KuMod,
    original: &str,
    dependency: &PackageDependency,
    development: bool,
) -> KuResult<String> {
    if manifest.name == dependency.name {
        return Err(error(
            "self_dependency",
            "a project cannot install itself as a dependency",
        ));
    }
    if let Some(existing) = manifest
        .direct_dependencies(false)
        .find(|dep| dep.name == dependency.name)
    {
        let old_development = manifest
            .development_dependencies
            .iter()
            .any(|dep| dep.name == dependency.name);
        if existing == dependency && old_development == development {
            return Ok(original.to_owned());
        }
        return Err(error("dependency_source_conflict", format!("'{}' is already declared with a different identity or scope; edit its ku.mod declaration explicitly", dependency.name)));
    }
    let group = if development { "dev" } else { "dep" };
    let mut output = original.to_owned();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&format!(
        "\n{group}.{} = {:?}\n",
        dependency.name, dependency.version
    ));
    if let Some(source) = &dependency.source {
        output.push_str(&format!(
            "{group}.{}.source = {source:?}\n",
            dependency.name
        ));
    }
    if let Some(checksum) = &dependency.checksum {
        output.push_str(&format!(
            "{group}.{}.checksum = {checksum:?}\n",
            dependency.name
        ));
    }
    if output.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(error(
            "manifest_limit",
            format!("ku.mod exceeds {MAX_MANIFEST_BYTES} bytes"),
        ));
    }
    Ok(output)
}

pub(super) fn validate_source(source: &str, span: Span) -> KuResult<()> {
    if source.len() > MAX_REGISTRY_URL_BYTES {
        return Err(KuError::package(
            "invalid_dependency_source",
            "dependency source is too long",
            span,
        ));
    }
    if file_url_path(source).is_some() {
        return Ok(());
    }
    if let Some(relative) = source.strip_prefix("path:") {
        validate_project_relative(relative, span)?;
        return Ok(());
    }
    if is_git_source(source) {
        GitSource::parse(source, true, span)?;
        return Ok(());
    }
    Err(KuError::package(
        "invalid_dependency_source",
        "dependency source must be absolute file://, project-relative path:, or pinned git+https://",
        span,
    ))
}

pub(super) fn validate_source_checksum(source: &str, checksum: &str, span: Span) -> KuResult<()> {
    if source.starts_with("file://") {
        validate_checksum(checksum, span)
    } else {
        validate_sha256_checksum(checksum, span)
    }
}

fn validate_project_relative(value: &str, span: Span) -> KuResult<()> {
    if value.is_empty() || value.len() > MAX_LOCK_DEPENDENCY_PATH_BYTES {
        return Err(KuError::package(
            "unsafe_local_dependency",
            "dependency path must be a bounded project-relative path",
            span,
        ));
    }
    let mut depth = 0;
    for component in value.split('/') {
        depth += 1;
        if depth > MAX_PACKAGE_DEPTH || component.is_empty() || matches!(component, "." | "..") {
            return Err(KuError::package(
                "unsafe_local_dependency",
                "dependency path must not contain empty components or traversal",
                span,
            ));
        }
        validate_portable_package_component(component, span)?;
    }
    Ok(())
}

fn portable_path(path: &Path) -> KuResult<String> {
    let value = path
        .to_str()
        .ok_or_else(|| {
            error(
                "non_portable_package_path",
                "dependency path is not valid UTF-8",
            )
        })?
        .replace(std::path::MAIN_SEPARATOR, "/");
    validate_project_relative(&value, Span::default())?;
    Ok(value)
}

pub(super) fn resolve_local_source(package: &PackageContext, source: &str) -> KuResult<PathBuf> {
    if let Some(relative) = source.strip_prefix("path:") {
        return resolve_relative_directory(&package.package_dir, relative);
    }
    let path = file_url_path(source).ok_or_else(|| {
        error(
            "unsupported_source",
            "expected a valid absolute file:// or project-relative path: dependency",
        )
    })?;
    fs::canonicalize(&path).map_err(|err| {
        error(
            "local_package_unavailable",
            format!(
                "cannot resolve dependency directory '{}': {err}",
                path.display()
            ),
        )
    })
}

fn resolve_relative_directory(root: &Path, relative: &str) -> KuResult<PathBuf> {
    validate_project_relative(relative, Span::default())?;
    let canonical_root = fs::canonicalize(root).map_err(|err| {
        error(
            "local_package_unavailable",
            format!("cannot resolve package root: {err}"),
        )
    })?;
    let mut path = canonical_root.clone();
    for component in relative.split('/') {
        path.push(component);
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            error(
                "local_package_unavailable",
                format!(
                    "cannot inspect dependency directory '{}': {err}",
                    path.display()
                ),
            )
        })?;
        if metadata.file_type().is_symlink()
            || archive_metadata_is_reparse_point(&metadata)
            || !metadata.is_dir()
        {
            return Err(error(
                "unsafe_local_dependency",
                "project-relative dependencies must use real directories inside the package",
            ));
        }
    }
    let actual = fs::canonicalize(&path).map_err(|err| {
        error(
            "local_package_unavailable",
            format!(
                "cannot resolve dependency directory '{}': {err}",
                path.display()
            ),
        )
    })?;
    if !actual.starts_with(&canonical_root) {
        return Err(error(
            "unsafe_local_dependency",
            "project-relative dependency escaped the package",
        ));
    }
    Ok(actual)
}

pub(super) fn resolve_nested_path(root: &Path, source: &str) -> KuResult<PathBuf> {
    let relative = source
        .strip_prefix("path:")
        .filter(|path| path.starts_with("src/"))
        .ok_or_else(|| {
            error(
                "unsupported_nested_dependency_source",
                "relative child packages must be within path:src/... of the parent package",
            )
        })?;
    resolve_relative_directory(root, relative)
}

pub(super) fn validate_nested_sources(manifest: &KuMod, span: Span) -> KuResult<()> {
    for child in &manifest.dependencies {
        if child.name == manifest.name
            || child.source.as_deref().is_some_and(|source| {
                !(source.starts_with("path:src/")
                    || (is_git_source(source) && child.checksum.is_some()))
            })
        {
            return Err(KuError::package(
                "unsupported_nested_dependency_source",
                format!(
                    "package '{}' dependency '{}' must use its registry, path:src/... within the packaged tree, or pinned HTTPS Git with a SHA-256 checksum; self dependencies are forbidden",
                    manifest.name, child.name
                ),
                span,
            ));
        }
    }
    Ok(())
}

pub(super) fn expand_file_dependency_graph(
    package: &PackageContext,
    direct: &[PackageDependency],
    resolved: &mut Vec<ResolvedFileDependency>,
    locked: &HashMap<String, LockedFileDependency>,
    mode: DependencyResolveMode,
    deadline: Instant,
    budget: &mut RegistryGraphResourceBudget,
) -> KuResult<()> {
    let mut owners = resolved
        .iter()
        .map(|dep| (dep.name.clone(), package.manifest.name.clone()))
        .collect::<HashMap<_, _>>();
    let direct_registry = direct
        .iter()
        .filter(|dep| dep.source.is_none())
        .map(|dep| dep.name.as_str())
        .collect::<HashSet<_>>();
    let mut cursor = 0;
    while cursor < resolved.len() {
        ensure_registry_deadline(deadline, Span::default())?;
        let parent = &resolved[cursor];
        let root = parent.package_root.clone();
        let parent_name = parent.name.clone();
        let dependencies = parent.dependencies.clone();
        cursor += 1;
        for dependency in &dependencies {
            let Some(source) = dependency.source.as_deref() else {
                continue;
            };
            if direct_registry.contains(dependency.name.as_str())
                || dependency.name == package.manifest.name
            {
                return Err(error(
                    "dependency_source_conflict",
                    format!("package '{parent_name}' requests '{}' from '{source}', conflicting with the consumer's explicit registry source or identity", dependency.name),
                ));
            }
            if let Some(existing) = resolved
                .iter()
                .find(|selected| selected.name == dependency.name)
            {
                let conflict = || {
                    error(
                    "dependency_source_conflict",
                    format!(
                        "packages '{}' and '{parent_name}' declare incompatible sources or versions for '{}'",
                        owners.get(&dependency.name).map(String::as_str).unwrap_or(&package.manifest.name),
                        dependency.name,
                    ),
                )
                };
                if existing.source != source
                    || dependency
                        .checksum
                        .as_ref()
                        .is_some_and(|checksum| checksum != &existing.checksum)
                    || !registry_version_satisfies_all(
                        &existing.version,
                        std::slice::from_ref(&dependency.version),
                        Span::default(),
                    )?
                {
                    return Err(conflict());
                }
                if source.starts_with("path:") {
                    let child = resolve_nested_path(&root, source)?;
                    if snapshot_for_source(&child, source, None)?.checksum != existing.checksum {
                        return Err(conflict());
                    }
                }
                continue;
            }
            if resolved.len() >= MAX_REGISTRY_DEPENDENCIES {
                return Err(error(
                    "dependency_limit",
                    "portable package graph has too many source dependencies",
                ));
            }
            budget.require_file_work_slots(1)?;
            let _slot = acquire_file_work_slot(&package.cache_dir, Span::default(), deadline)?;
            let child = resolve_file_dependency_with_mode(
                package,
                dependency,
                source,
                mode,
                locked.get(&dependency.name),
                deadline,
                Some(&root),
            )?;
            budget.record_file_snapshot(child.snapshot_bytes)?;
            owners.insert(child.name.clone(), parent_name.clone());
            resolved.push(child);
        }
    }
    Ok(())
}

pub(super) fn is_git_source(source: &str) -> bool {
    source.starts_with("git+https://")
}

struct GitSource {
    url: Url,
    revision: String,
    subdirectory: Option<String>,
}

impl GitSource {
    fn parse(source: &str, locked: bool, span: Span) -> KuResult<Self> {
        let request = source.strip_prefix("git+").unwrap_or(source);
        let (url, selection) = request.split_once('#').unwrap_or((request, "HEAD"));
        let (revision, subdirectory) = selection
            .split_once(':')
            .map_or((selection, None), |(revision, subdirectory)| {
                (revision, Some(subdirectory))
            });
        let invalid = || {
            KuError::package(
            "invalid_git_source",
            "use an HTTPS Git URL without credentials or query, optionally #tag, #full-commit or #full-commit:package/subdirectory",
            span,
        )
        };
        if request.len() > MAX_REGISTRY_URL_BYTES || request.chars().any(char::is_control) {
            return Err(invalid());
        }
        let url = Url::parse(url).map_err(|_| invalid())?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || url.path().is_empty()
            || url.path() == "/"
            || url.password().is_some()
            || !url.username().is_empty()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.as_str().contains('"')
        {
            return Err(invalid());
        }
        if revision.is_empty()
            || revision.len() > 128
            || revision.starts_with('-')
            || revision.starts_with('/')
            || revision.contains("..")
            || revision.ends_with('/')
            || revision.contains("//")
            || !revision
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"_-/.".contains(&ch))
            || (locked && !full_commit(revision))
        {
            return Err(invalid());
        }
        if let Some(subdirectory) = subdirectory {
            validate_project_relative(subdirectory, span)?;
        }
        Ok(Self {
            url,
            revision: revision.to_owned(),
            subdirectory: subdirectory.map(str::to_owned),
        })
    }

    fn pin(&self, commit: &str) -> String {
        let mut source = format!("git+{}#{commit}", self.url);
        if let Some(subdirectory) = &self.subdirectory {
            source.push(':');
            source.push_str(subdirectory);
        }
        source
    }
}

fn full_commit(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|ch| ch.is_ascii_hexdigit())
}

pub(super) fn restore_git_dependency(
    project: &PackageContext,
    dependency: &PackageDependency,
    source: &str,
    locked: Option<&LockedFileDependency>,
    deadline: Instant,
) -> KuResult<ResolvedFileDependency> {
    let git = GitSource::parse(source, true, Span::default())?;
    let locked = locked.filter(|lock| {
        lock.name == dependency.name
            && lock.requirement == dependency.version
            && lock.source == source
    });
    let required = dependency
        .checksum
        .as_deref()
        .or_else(|| locked.map(|lock| lock.checksum.as_str()));
    let exact_version = parse_package_version(&dependency.version, Span::default()).is_ok();
    if let Some(checksum) = required {
        if let Some(version) = locked
            .map(|lock| lock.version.as_str())
            .or_else(|| exact_version.then_some(dependency.version.as_str()))
        {
            let cache_key = file_cache_key(&dependency.name, version, checksum);
            let target = file_dependency_cache_root(project, &dependency.name, &cache_key);
            if let Some(snapshot) = installed_file_cache_snapshot(
                &target,
                dependency,
                version,
                checksum,
                Span::default(),
            )? {
                let identity =
                    inspect_file_dependency_identity(&target, dependency, Span::default())?;
                return Ok(resolved_file_dependency(
                    dependency,
                    source,
                    identity.version,
                    checksum.to_owned(),
                    cache_key,
                    target,
                    identity.dependencies,
                    snapshot.bytes,
                ));
            }
        }
    }

    let (_work, root, commit) = fetch_git_package(project, &git, deadline)?;
    if !commit.eq_ignore_ascii_case(&git.revision) {
        return Err(error(
            "git_commit_mismatch",
            format!(
                "Git returned commit '{commit}' instead of pinned '{}'",
                git.revision
            ),
        ));
    }
    let identity = inspect_file_dependency_identity(&root, dependency, Span::default())?;
    let snapshot = snapshot_for_source(&root, source, None)?;
    if required.is_some_and(|checksum| checksum != snapshot.checksum)
        || locked.is_some_and(|lock| lock.version != identity.version)
    {
        return Err(error(
            "locked_source_changed",
            format!(
                "Git package '{}' does not match its pinned content",
                dependency.name
            ),
        ));
    }
    install_file_dependency(
        project, dependency, source, &root, identity, snapshot, deadline,
    )
}

fn fetch_git_package(
    project: &PackageContext,
    git: &GitSource,
    deadline: Instant,
) -> KuResult<(TemporaryDirectory, PathBuf, String)> {
    let scratch = project
        .cache_dir
        .join(format!(".git-source-{}", operation_nonce()));
    fs::create_dir(&scratch).map_err(|err| {
        error(
            "git_staging_failed",
            format!("cannot create Git staging directory: {err}"),
        )
    })?;
    let work = TemporaryDirectory::new(scratch.clone());
    let repository = scratch.join("repo");
    fs::create_dir(&repository).map_err(|err| {
        error(
            "git_staging_failed",
            format!("cannot create a temporary Git repository: {err}"),
        )
    })?;
    invoke_git(
        &repository,
        &["init", "--bare", "--quiet", "--template="],
        deadline,
        4096,
    )?;
    invoke_git(
        &repository,
        &[
            "fetch",
            "--quiet",
            "--depth=1",
            "--no-tags",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--",
            git.url.as_str(),
            &git.revision,
        ],
        deadline,
        4096,
    )?;
    reject_oversize_git_objects(&repository)?;
    let commit = invoke_git(
        &repository,
        &["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
        deadline,
        64,
    )?;
    let commit = std::str::from_utf8(&commit)
        .ok()
        .map(str::trim)
        .filter(|text| full_commit(text))
        .ok_or_else(|| {
            error(
                "invalid_git_commit",
                "server did not provide a full Git commit",
            )
        })?;
    if full_commit(&git.revision) && !commit.eq_ignore_ascii_case(&git.revision) {
        return Err(error(
            "git_commit_mismatch",
            format!(
                "resolved Git commit '{commit}' does not match '{}'",
                git.revision
            ),
        ));
    }
    let tree = match &git.subdirectory {
        Some(subdirectory) => format!("{commit}:{subdirectory}"),
        None => commit.to_owned(),
    };
    let output = invoke_git(
        &repository,
        &["ls-tree", "--name-only", "-z", &tree],
        deadline,
        64 * 1024,
    )?;
    let files = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    if !files.contains(&MANIFEST_FILE.as_bytes())
        || !files.contains(&DEFAULT_IMPORT_ROOT.as_bytes())
    {
        return Err(error("missing_package_manifest", "selected Git package directory must contain ku.mod and src/; add #ref:package/subdirectory for nested packages"));
    }
    let mut args = vec![
        "archive",
        "--format=tar",
        "--prefix=package/",
        &tree,
        "--",
        MANIFEST_FILE,
        DEFAULT_IMPORT_ROOT,
    ];
    for legal in LEGAL_FILES {
        if files.contains(&legal.as_bytes()) {
            args.push(legal);
        }
    }
    let archive = invoke_git(&repository, &args, deadline, MAX_GIT_ARCHIVE_BYTES)?;
    let root = scratch.join("package");
    unpack_git_package(&archive, &root, deadline)?;
    Ok((work, root, commit.to_owned()))
}

fn invoke_git(
    repository: &Path,
    args: &[&str],
    deadline: Instant,
    limit: usize,
) -> KuResult<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "http.followRedirects=false",
            "-c",
            "fetch.fsckObjects=true",
            "-c",
            "fetch.unpackLimit=1",
            "-c",
            "fetch.writeCommitGraph=false",
            "-c",
            "core.hooksPath=",
            "-c",
            "credential.helper=",
            "--no-pager",
            "--no-optional-locks",
        ])
        .args(args)
        .stdin(Stdio::null());
    // Never run helpers injected via another repository, user templates, or
    // inherited GIT_CONFIG/alternate-object variables.
    for (name, _) in env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    #[cfg(unix)]
    if args[0] == "fetch" {
        limit_git_pack_file(&mut command);
    }
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(120));
    let (status, output) =
        crate::cli::run_build_process_capture_stdout(&mut command, timeout, limit).map_err(
            |err| {
                error(
                    "git_command_failed",
                    format!("Git {} failed: {err}", args[0]),
                )
            },
        )?;
    if !status.success() {
        return Err(error("git_command_failed", format!("Git {} failed ({status}); check the HTTPS source, noninteractive access, object limit, and requested ref", args[0])));
    }
    Ok(output)
}

#[cfg(unix)]
fn limit_git_pack_file(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // fetch.unpackLimit=1 confines incoming objects to the shallow fetch pack
    // and index files. Keep Git and its children from writing an oversized pack.
    unsafe {
        command.pre_exec(|| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            let cap = MAX_REGISTRY_DOWNLOAD_BYTES as libc::rlim_t;
            limit.rlim_cur = limit.rlim_cur.min(cap);
            limit.rlim_max = limit.rlim_max.min(cap);
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn reject_oversize_git_objects(repository: &Path) -> KuResult<()> {
    let limit = || {
        error(
            "git_download_limit",
            "shallow Git object storage exceeds the 32 MB package transfer budget",
        )
    };
    let mut pending = vec![repository.join("objects")];
    let mut entries = 0usize;
    let mut bytes = 0u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|err| {
            error(
                "git_staging_failed",
                format!("cannot inspect Git object storage: {err}"),
            )
        })? {
            entries += 1;
            if entries > MAX_REGISTRY_GRAPH_INDEX_ITEMS {
                return Err(limit());
            }
            let path = entry
                .map_err(|err| {
                    error(
                        "git_staging_failed",
                        format!("cannot inspect Git object entry: {err}"),
                    )
                })?
                .path();
            let meta = fs::symlink_metadata(&path).map_err(|err| {
                error(
                    "git_staging_failed",
                    format!("cannot inspect Git object metadata: {err}"),
                )
            })?;
            if meta.file_type().is_symlink() || archive_metadata_is_reparse_point(&meta) {
                return Err(error(
                    "git_staging_failed",
                    "Git object storage must not contain links",
                ));
            }
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() {
                bytes = bytes.saturating_add(meta.len());
                if bytes > MAX_REGISTRY_DOWNLOAD_BYTES {
                    return Err(limit());
                }
            } else {
                return Err(error(
                    "git_staging_failed",
                    "Git object storage contains a special file",
                ));
            }
        }
    }
    Ok(())
}

fn unpack_git_package(archive: &[u8], output: &Path, deadline: Instant) -> KuResult<()> {
    let mut archive = tar::Archive::new(archive);
    let policy = PackageArchivePolicy::default();
    let mut root_name = None;
    let mut seen = HashSet::new();
    let mut total = 0u64;
    fs::create_dir(output).map_err(|err| {
        error(
            "git_staging_failed",
            format!("cannot create snapshot directory: {err}"),
        )
    })?;
    let entries = archive.entries().map_err(|err| {
        error(
            "invalid_git_archive",
            format!("invalid Git tree archive: {err}"),
        )
    })?;
    for entry in entries {
        ensure_registry_deadline(deadline, Span::default())?;
        let mut entry = entry.map_err(|err| {
            error(
                "invalid_git_archive",
                format!("invalid Git tree entry: {err}"),
            )
        })?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(error(
                "unsupported_git_entry",
                "Git packages must not contain links, submodules or special files",
            ));
        }
        let path = entry.path().map_err(|err| {
            error(
                "invalid_git_archive",
                format!("invalid Git tree path: {err}"),
            )
        })?;
        let relative = validate_archive_entry_path(&path, &mut root_name, policy, Span::default())?;
        if relative.as_os_str().is_empty() && kind.is_dir() {
            continue;
        }
        let portable = portable_path(&relative)?;
        if root_name.as_deref() != Some("package")
            || !seen.insert(portable.to_lowercase())
            || seen.len() > MAX_PACKAGE_FILES
        {
            return Err(error(
                "git_archive_limit",
                "Git package has an unexpected root, conflicting paths, or too many entries",
            ));
        }
        let target = output.join(relative);
        if kind.is_dir() {
            fs::create_dir_all(&target).map_err(|err| {
                error(
                    "git_staging_failed",
                    format!("cannot create package directory: {err}"),
                )
            })?;
        } else {
            total = total.saturating_add(entry.size());
            if total > MAX_PACKAGE_BYTES {
                return Err(error("git_archive_limit", "Git package tree exceeds 10 MB"));
            }
            let parent = target
                .parent()
                .expect("Git package files are below the snapshot");
            fs::create_dir_all(parent).map_err(|err| {
                error(
                    "git_staging_failed",
                    format!("cannot create package directory: {err}"),
                )
            })?;
            let mut staged = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|err| {
                    error(
                        "git_staging_failed",
                        format!("cannot create snapshot file: {err}"),
                    )
                })?;
            let expected = entry.size();
            let actual = io::copy(&mut entry, &mut staged).map_err(|err| {
                error(
                    "invalid_git_archive",
                    format!("cannot extract Git package file: {err}"),
                )
            })?;
            if actual != expected {
                return Err(error(
                    "invalid_git_archive",
                    "Git package file is truncated",
                ));
            }
        }
    }
    Ok(())
}

fn install_receipts(
    project: &PackageContext,
    names: &HashSet<&str>,
    deadline: Instant,
) -> KuResult<Vec<PackageInstallReceipt>> {
    let mut receipts = Vec::new();
    let resolved = project
        .resolved_file_dependencies
        .iter()
        .map(|dep| (&dep.name, &dep.version, &dep.source, &dep.package_root))
        .chain(
            project
                .resolved_registry_dependencies
                .iter()
                .map(|dep| (&dep.name, &dep.version, &dep.url, &dep.package_root)),
        );
    for (name, version, source, root) in resolved {
        if !names.contains(name.as_str()) {
            continue;
        }
        receipts.push(PackageInstallReceipt {
            name: name.clone(),
            version: version.clone(),
            source: source.clone(),
            package_root: root.clone(),
            imports: package_imports(root, name, deadline)?,
        });
    }
    receipts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(receipts)
}

fn package_imports(
    root: &Path,
    package: &str,
    deadline: Instant,
) -> KuResult<Vec<PackageInstallModule>> {
    let import_root = root.join(DEFAULT_IMPORT_ROOT);
    let mut pending = vec![import_root.clone()];
    let policy = PackageArchivePolicy::default();
    let mut imports = Vec::new();
    let mut count = 0usize;
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|err| {
            error(
                "invalid_import_root",
                format!("cannot list package '{package}' import root: {err}"),
            )
        })?;
        for entry in entries {
            ensure_registry_deadline(deadline, Span::default())?;
            count += 1;
            if count > policy.max_files {
                return Err(error(
                    "import_inventory_limit",
                    "package import inventory exceeds its entry limit",
                ));
            }
            let entry = entry.map_err(|err| {
                error(
                    "invalid_import_root",
                    format!("cannot list package module: {err}"),
                )
            })?;
            let metadata = entry.file_type().map_err(|err| {
                error(
                    "invalid_import_root",
                    format!("cannot inspect package module: {err}"),
                )
            })?;
            let path = entry.path();
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file()
                || path.extension().and_then(|extension| extension.to_str()) != Some("ku")
            {
                continue;
            }
            let file = fs::File::open(&path).map_err(|err| {
                error(
                    "invalid_package_module",
                    format!("cannot open Ku module: {err}"),
                )
            })?;
            let mut source = String::new();
            file.take(policy.max_file_bytes + 1)
                .read_to_string(&mut source)
                .map_err(|err| {
                    error(
                        "invalid_package_module",
                        format!("cannot read Ku module: {err}"),
                    )
                })?;
            if source.len() as u64 > policy.max_file_bytes {
                return Err(error(
                    "import_inventory_limit",
                    "Ku module exceeds the package size budget",
                ));
            }
            // Invalid optional files do not imply a valid export. Runtime and
            // compile checks report their exact parse error if actually imported.
            let Ok(tokens) = Lexer::new(&source).tokenize() else {
                continue;
            };
            let Ok(program) = Parser::new(tokens).parse_program() else {
                continue;
            };
            let mut exports = program
                .items
                .into_iter()
                .filter_map(|item| {
                    let Item::Function(function) = item else {
                        return None;
                    };
                    function
                        .name
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_uppercase)
                        .then_some(function.name)
                })
                .collect::<Vec<_>>();
            if exports.is_empty() {
                continue;
            }
            exports.sort_unstable();
            let module = path
                .strip_prefix(&import_root)
                .expect("inventory walks only inside the root")
                .with_extension("");
            let path = format!("@{package}/{}", portable_path(&module)?);
            imports.push(PackageInstallModule { path, exports });
        }
    }
    imports.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(imports)
}

fn read_optional_regular(path: &Path, limit: u64) -> KuResult<Option<Vec<u8>>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(error(
                "install_io",
                format!("cannot inspect '{}': {err}", path.display()),
            ))
        }
    };
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || archive_metadata_is_reparse_point(&meta)
        || meta.len() > limit
    {
        return Err(error(
            "install_io",
            format!(
                "'{}' must be a plain file at most {limit} bytes",
                path.display()
            ),
        ));
    }
    let file = fs::File::open(path).map_err(|err| {
        error(
            "install_io",
            format!("cannot open '{}': {err}", path.display()),
        )
    })?;
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data).map_err(|err| {
        error(
            "install_io",
            format!("cannot read '{}': {err}", path.display()),
        )
    })?;
    if data.len() as u64 > limit {
        return Err(error(
            "install_io",
            format!("'{}' is larger than {limit} bytes", path.display()),
        ));
    }
    Ok(Some(data))
}

fn replace_contents(path: &Path, contents: &[u8]) -> KuResult<()> {
    let stage_path = path.with_file_name(format!(".ku-install-{}.tmp", operation_nonce()));
    let _cleanup = TemporaryFile::new(stage_path.clone());
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&stage_path)
        .map_err(|err| {
            error(
                "install_io",
                format!("cannot create install staging file: {err}"),
            )
        })?;
    if let Ok(meta) = fs::symlink_metadata(path) {
        file.set_permissions(meta.permissions()).map_err(|err| {
            error(
                "install_io",
                format!("cannot preserve '{}' permissions: {err}", path.display()),
            )
        })?;
    }
    file.write_all(contents)
        .and_then(|_| file.sync_all())
        .map_err(|err| {
            error(
                "install_io",
                format!("cannot write install staging file: {err}"),
            )
        })?;
    drop(file);
    replace_file_atomically(&stage_path, path).map_err(|err| {
        error(
            "install_io",
            format!("cannot replace '{}': {err}", path.display()),
        )
    })?;
    sync_parent_directory(path)
}

fn lock_digest(value: Option<&[u8]>) -> [u8; 33] {
    let mut identity = [0u8; 33];
    if let Some(value) = value {
        identity[0] = 1;
        identity[1..].copy_from_slice(&Sha256::digest(value));
    }
    identity
}

fn transaction_bytes(
    old_manifest: &[u8],
    old_lock: Option<&[u8]>,
    manifest: &[u8],
    lock: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(TRANSACTION_MAGIC.len() + 74 + manifest.len() + lock.len());
    bytes.extend_from_slice(TRANSACTION_MAGIC);
    bytes.extend_from_slice(&lock_digest(Some(old_manifest)));
    bytes.extend_from_slice(&lock_digest(old_lock));
    bytes.extend_from_slice(&(manifest.len() as u64).to_le_bytes());
    bytes.extend_from_slice(manifest);
    bytes.extend_from_slice(lock);
    bytes
}

struct InstallTransaction<'a> {
    before_manifest: &'a [u8],
    before_lock: &'a [u8],
    manifest: &'a [u8],
    lock: &'a [u8],
}

fn parse_transaction(bytes: &[u8]) -> KuResult<InstallTransaction<'_>> {
    let invalid = || {
        error("install_recovery", "ku.lock.install-transaction is invalid; retain it while diagnosing the interrupted install")
    };
    let bytes = bytes.strip_prefix(TRANSACTION_MAGIC).ok_or_else(invalid)?;
    if bytes.len() < 74 {
        return Err(invalid());
    }
    let length = u64::from_le_bytes(bytes[66..74].try_into().expect("eight length bytes"));
    if length > MAX_MANIFEST_BYTES || length > (bytes.len() - 74) as u64 {
        return Err(invalid());
    }
    let (manifest, lock) = bytes[74..].split_at(length as usize);
    if lock.len() as u64 > MAX_PACKAGE_LOCK_BYTES
        || lock.is_empty()
        || std::str::from_utf8(manifest)
            .ok()
            .and_then(|manifest| parse_manifest(manifest, Span::default()).ok())
            .is_none()
    {
        return Err(invalid());
    }
    Ok(InstallTransaction {
        before_manifest: &bytes[..33],
        before_lock: &bytes[33..66],
        manifest,
        lock,
    })
}

pub(super) fn restore_transaction_locked(manifest_path: &Path) -> KuResult<()> {
    let journal = manifest_path.with_file_name(INSTALL_TRANSACTION);
    let lock_path = manifest_path.with_file_name(LOCK_FILE);
    let Some(bytes) =
        read_optional_regular(&journal, MAX_MANIFEST_BYTES + MAX_PACKAGE_LOCK_BYTES + 128)?
    else {
        return Ok(());
    };
    let txn = parse_transaction(&bytes)?;
    let current_manifest = read_optional_regular(manifest_path, MAX_MANIFEST_BYTES)?;
    let current_lock = read_optional_regular(&lock_path, MAX_PACKAGE_LOCK_BYTES)?;
    for (current, before, after) in [
        (
            current_manifest.as_deref(),
            txn.before_manifest,
            txn.manifest,
        ),
        (current_lock.as_deref(), txn.before_lock, txn.lock),
    ] {
        if current != Some(after) && lock_digest(current).as_slice() != before {
            return Err(error("install_recovery_conflict", "ku.mod or ku.lock changed after an interrupted install; recovery refused to overwrite unrelated changes"));
        }
    }
    if current_lock.as_deref() != Some(txn.lock) {
        replace_contents(&lock_path, txn.lock)?;
    }
    if current_manifest.as_deref() != Some(txn.manifest) {
        replace_contents(manifest_path, txn.manifest)?;
    }
    fs::remove_file(&journal).map_err(|err| {
        error(
            "install_recovery",
            format!("cannot remove completed install journal: {err}"),
        )
    })?;
    sync_parent_directory(&journal)
}

pub(super) fn recover_transaction_if_needed(manifest_path: &Path) -> KuResult<()> {
    let journal = manifest_path.with_file_name(INSTALL_TRANSACTION);
    match fs::symlink_metadata(&journal) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(error(
                "install_recovery",
                format!("cannot inspect install journal: {err}"),
            ))
        }
        Ok(_) => {}
    }
    let lock_path = package_lock_io_path(&manifest_path.with_file_name(LOCK_FILE));
    let _guard = acquire_registry_install_lock(&lock_path, Span::default())?;
    restore_transaction_locked(manifest_path)
}

fn commit_install(
    project: &PackageContext,
    original: &str,
    old_lock: Option<&[u8]>,
    manifest: &str,
    lock: &[u8],
    deadline: Instant,
) -> KuResult<()> {
    validate_lock_output(lock)?;
    let io_lock = package_lock_io_path(&project.lock_path);
    let _guard = acquire_registry_install_lock_until(&io_lock, Span::default(), deadline)?;
    restore_transaction_locked(&project.manifest_path)?;
    if read_manifest_file(&project.manifest_path)? != original
        || read_optional_regular(&project.lock_path, MAX_PACKAGE_LOCK_BYTES)?.as_deref() != old_lock
    {
        return Err(error("manifest_changed", "ku.mod or ku.lock changed during install; retry so the other writer's changes are preserved"));
    }
    if original == manifest {
        return replace_contents(&project.lock_path, lock);
    }
    let journal = project.package_dir.join(INSTALL_TRANSACTION);
    for target in [&project.manifest_path, &project.lock_path] {
        if fs::metadata(target).is_ok_and(|meta| meta.permissions().readonly()) {
            return Err(error(
                "install_io",
                format!(
                    "'{}' is read-only; neither ku.mod nor ku.lock was written",
                    target.display()
                ),
            ));
        }
    }
    let bytes = transaction_bytes(original.as_bytes(), old_lock, manifest.as_bytes(), lock);
    replace_contents(&journal, &bytes)?;
    restore_transaction_locked(&project.manifest_path)
}

pub(super) const LEGAL_FILES: &[&str] = &[
    "LICENSE",
    "LICENSE.md",
    "LICENSE.txt",
    "NOTICE",
    "NOTICE.md",
    "NOTICE.txt",
];

pub(super) fn validate_interop_sources(root: &Path, span: Span) -> KuResult<()> {
    let import_root = root.join(DEFAULT_IMPORT_ROOT);
    let descriptor = import_root.join("ku.interop");
    let policy = PackageArchivePolicy::default();
    let payload = read_optional_regular(&descriptor, MAX_MANIFEST_BYTES)?.ok_or_else(|| {
        KuError::package(
            "missing_interop_descriptor",
            "interop.version = \"1\" requires src/ku.interop",
            span,
        )
    })?;
    let source = std::str::from_utf8(&payload).map_err(|_| {
        KuError::package(
            "invalid_interop_descriptor",
            "src/ku.interop must be UTF-8 text",
            span,
        )
    })?;
    let kind = source
        .lines()
        .map(without_comment)
        .find(|line| !line.is_empty());
    if !matches!(kind, Some("kind = \"native\"" | "kind = \"python\"")) {
        return Err(KuError::package(
            "invalid_interop_descriptor",
            "src/ku.interop must declare native or python kind",
            span,
        ));
    }
    let mut directories = vec![root.to_path_buf()];
    let mut entries = 0usize;
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).map_err(|err| {
            KuError::package(
                "invalid_interop_package",
                format!("failed to read source-only interop package: {err}"),
                span,
            )
        })? {
            entries += 1;
            if entries > policy.max_files {
                return Err(KuError::package(
                    "invalid_interop_package",
                    "interop source package has too many entries",
                    span,
                ));
            }
            let entry = entry.map_err(|err| {
                KuError::package(
                    "invalid_interop_package",
                    format!("failed to read interop package entry: {err}"),
                    span,
                )
            })?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("walker stays inside interop root");
            if relative.components().count() == 1
                && validate_archive_top_level(relative, span).is_err()
            {
                continue;
            }
            if package_publish_path_is_excluded(&path) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|err| {
                KuError::package(
                    "invalid_interop_package",
                    format!("failed to inspect interop source: {err}"),
                    span,
                )
            })?;
            if metadata.file_type().is_symlink() || archive_metadata_is_reparse_point(&metadata) {
                return Err(KuError::package(
                    "invalid_interop_package",
                    "interop source packages must not contain links",
                    span,
                ));
            }
            if metadata.is_dir() {
                directories.push(path);
                continue;
            }
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            let source_extension = matches!(
                extension,
                "c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "ipp" | "inc" | "py" | "pyi"
            );
            let text_extension = matches!(extension, "ku" | "md" | "txt" | "rst" | "json");
            if !metadata.is_file()
                || package_publish_file_is_forbidden(&path)
                || matches!(
                    name,
                    "setup.py" | "build.py" | "CMakeLists.txt" | "meson.build"
                )
                || !(path == descriptor
                    || name == MANIFEST_FILE
                    || matches!(name, "README" | "COPYING")
                    || LEGAL_FILES.contains(&name)
                    || text_extension
                    || (path.starts_with(&import_root) && source_extension))
            {
                return Err(KuError::package(
                    "unsupported_interop_package_file",
                    format!(
                        "'{}' is outside the source-only interop package format",
                        path.strip_prefix(root).unwrap_or(&path).display()
                    ),
                    span,
                ));
            }
            if read_optional_regular(&path, policy.max_file_bytes)?
                .is_none_or(|bytes| bytes.contains(&0) || std::str::from_utf8(&bytes).is_err())
            {
                return Err(KuError::package(
                    "invalid_interop_package",
                    "interop package source files must be plain UTF-8 text",
                    span,
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
