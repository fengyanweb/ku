use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
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
    pub registry: Option<PackageRegistry>,
    pub dependencies: Vec<PackageDependency>,
}

#[cfg(test)]
mod package_process_lock_tests {
    use super::*;

    const HELPER_MODE_ENV: &str = "KU_TEST_PACKAGE_PROCESS_HELPER_MODE";
    const HELPER_TARGET_ENV: &str = "KU_TEST_PACKAGE_PROCESS_HELPER_TARGET";
    const HELPER_READY_ENV: &str = "KU_TEST_PACKAGE_PROCESS_HELPER_READY";
    const HELPER_RELEASE_ENV: &str = "KU_TEST_PACKAGE_PROCESS_HELPER_RELEASE";

    struct TestChildProcess {
        child: Option<std::process::Child>,
        release: PathBuf,
    }

    impl TestChildProcess {
        fn terminate(&mut self) {
            let Some(mut child) = self.child.take() else {
                return;
            };
            if child
                .try_wait()
                .expect("inspect package lock helper")
                .is_none()
            {
                child.kill().expect("terminate package lock helper");
            }
            child.wait().expect("reap package lock helper");
        }

        fn release_and_wait(&mut self) {
            fs::write(&self.release, b"release").expect("signal package lock helper");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let child = self.child.as_mut().expect("package lock helper exists");
                if let Some(status) = child.try_wait().expect("inspect package lock helper") {
                    self.child = None;
                    assert!(status.success(), "package lock helper failed: {status}");
                    return;
                }
                if Instant::now() >= deadline {
                    self.terminate();
                    panic!("package lock helper did not exit after its release signal");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for TestChildProcess {
        fn drop(&mut self) {
            self.terminate();
        }
    }

    fn process_test_root(label: &str) -> PathBuf {
        env::temp_dir().join(format!("ku-{label}-{}", operation_nonce()))
    }

    fn spawn_lock_helper(mode: &str, target: &Path, ready: &Path) -> TestChildProcess {
        let release = ready.with_extension("release");
        let child = std::process::Command::new(
            env::current_exe().expect("locate current package unit-test executable"),
        )
        .arg("package_process_lock_holder_helper")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(HELPER_MODE_ENV, mode)
        .env(HELPER_TARGET_ENV, target)
        .env(HELPER_READY_ENV, ready)
        .env(HELPER_RELEASE_ENV, &release)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn package lock helper process");
        TestChildProcess {
            child: Some(child),
            release,
        }
    }

    fn wait_for_helper(child: &mut TestChildProcess, ready: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if ready.is_file() {
                return;
            }
            if let Some(status) = child
                .child
                .as_mut()
                .expect("package lock helper exists")
                .try_wait()
                .expect("inspect package lock helper")
            {
                child.child = None;
                panic!("package lock helper exited before becoming ready: {status}");
            }
            if Instant::now() >= deadline {
                child.terminate();
                panic!("package lock helper did not become ready within five seconds");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn slot_test_plan(cache_dir: &Path) -> RegistryDownloadPlan {
        let checksum = format!("sha256-{}", "a".repeat(64));
        RegistryDownloadPlan {
            cache_dir: cache_dir.to_path_buf(),
            name: "slotpkg".to_string(),
            version: "1.0.0".to_string(),
            url: "https://registry.example/slotpkg-1.0.0.tar.zst".to_string(),
            checksum: checksum.clone(),
            target_dir: cache_dir.join(PACKAGE_CACHE_DIR).join("slotpkg").join(
                registry_cache_key_for_identity("slotpkg", "1.0.0", &checksum),
            ),
            temporary_dir: cache_dir
                .join(".registry-downloads")
                .join(format!("slotpkg-1.0.0-{}", operation_nonce())),
            action: RegistryCacheAction::DownloadAndReplace,
            policy: RegistryFetchPolicy::default(),
        }
    }

    fn hold_helper_guard_until_release(ready: &Path, release: &Path) {
        fs::write(ready, b"ready").expect("publish package lock helper readiness");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !release.is_file() {
            assert!(
                Instant::now() < deadline,
                "package lock helper exceeded its bounded lifetime"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn package_process_lock_holder_helper() {
        let Some(mode) = env::var_os(HELPER_MODE_ENV) else {
            return;
        };
        let target = PathBuf::from(
            env::var_os(HELPER_TARGET_ENV).expect("package lock helper target is configured"),
        );
        let ready = PathBuf::from(
            env::var_os(HELPER_READY_ENV).expect("package lock helper ready path is configured"),
        );
        let release = PathBuf::from(
            env::var_os(HELPER_RELEASE_ENV)
                .expect("package lock helper release path is configured"),
        );
        match mode.to_str().expect("package lock helper mode is UTF-8") {
            "install" => {
                let _guard = acquire_registry_install_lock_until(
                    &target,
                    Span::default(),
                    Instant::now() + Duration::from_secs(10),
                )
                .expect("child process acquires install lock");
                hold_helper_guard_until_release(&ready, &release);
            }
            "slot" => {
                let plan = slot_test_plan(&target);
                fs::create_dir_all(
                    plan.target_dir
                        .parent()
                        .expect("slot plan target has a parent"),
                )
                .expect("create child slot cache");
                let _guard = acquire_registry_download_slot(
                    &plan,
                    Span::default(),
                    Instant::now() + Duration::from_secs(10),
                )
                .expect("child process acquires registry download slot");
                hold_helper_guard_until_release(&ready, &release);
            }
            "file-slot" => {
                fs::create_dir_all(&target).expect("create child file slot cache");
                let _guard = acquire_file_work_slot(
                    &target,
                    Span::default(),
                    Instant::now() + Duration::from_secs(10),
                )
                .expect("child process acquires file dependency work slot");
                hold_helper_guard_until_release(&ready, &release);
            }
            "usage" => {
                let _guard = acquire_package_usage_lease_for_cache_until(
                    &target,
                    Instant::now() + Duration::from_secs(10),
                )
                .expect("child process acquires shared package usage lease");
                hold_helper_guard_until_release(&ready, &release);
            }
            other => panic!("unknown package lock helper mode '{other}'"),
        }
    }

    #[test]
    fn operating_system_lock_is_cross_process_and_released_after_termination() {
        let root = process_test_root("cross-process-install-lock");
        fs::create_dir_all(&root).expect("create cross-process lock root");
        let lock_path = root.join("install.lock");
        let ready = root.join("holder.ready");
        let mut holder = spawn_lock_helper("install", &lock_path, &ready);
        wait_for_helper(&mut holder, &ready);

        assert!(
            try_acquire_registry_install_lock(&lock_path)
                .expect("probe child-held install lock")
                .is_none(),
            "a different process must not acquire the held install lock"
        );
        holder.terminate();
        let recovered = acquire_registry_install_lock_until(
            &lock_path,
            Span::default(),
            Instant::now() + Duration::from_secs(2),
        )
        .expect("the OS must release an install lock after process termination");
        drop(recovered);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_operation_lock_rejects_links_without_touching_the_target() {
        let root = process_test_root("linked-package-operation-lock");
        fs::create_dir_all(&root).expect("create linked lock test root");
        let external = root.join("external");
        fs::write(&external, b"sentinel").expect("write external lock target");
        let lock_path = root.join("install.lock");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&external, &lock_path).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&external, &lock_path).is_ok();
        if linked {
            let error = match try_acquire_registry_install_lock(&lock_path) {
                Ok(_) => panic!("a linked package operation lock must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.code.as_deref(), Some("unsafe_cache_path"));
            assert_eq!(
                fs::read(&external).expect("read external lock target"),
                b"sentinel"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blocking_package_operation_lock_rejects_links_without_touching_the_target() {
        let root = process_test_root("linked-blocking-package-operation-lock");
        fs::create_dir_all(&root).expect("create linked blocking lock test root");
        let external = root.join("external");
        fs::write(&external, b"sentinel").expect("write external blocking lock target");
        let lock_path = root.join("install.lock");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&external, &lock_path).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&external, &lock_path).is_ok();
        if linked {
            let error = match acquire_registry_install_lock_until(
                &lock_path,
                Span::default(),
                Instant::now() + Duration::from_secs(1),
            ) {
                Ok(_) => panic!("a linked blocking package operation lock must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.code.as_deref(), Some("unsafe_cache_path"));
            assert_eq!(
                fs::read(&external).expect("read external blocking lock target"),
                b"sentinel"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_usage_lease_rejects_links_without_touching_the_target() {
        let root = process_test_root("linked-package-usage-lock");
        let cache = root.join("cache");
        fs::create_dir_all(&cache).expect("create linked usage lock cache");
        let external = root.join("external");
        fs::write(&external, b"sentinel").expect("write external usage lock target");
        let usage_lock = cache.join(".package-usage.lock");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&external, &usage_lock).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&external, &usage_lock).is_ok();
        if linked {
            let error = match acquire_package_usage_lease_for_cache_until(
                &cache,
                Instant::now() + Duration::from_secs(1),
            ) {
                Ok(_) => panic!("a linked package usage lock must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.code.as_deref(), Some("unsafe_cache_path"));
            assert_eq!(
                fs::read(&external).expect("read external usage lock target"),
                b"sentinel"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_download_slots_are_global_across_processes() {
        let root = process_test_root("cross-process-registry-slots");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&cache_dir).expect("create shared registry slot cache");
        let mut holders = Vec::new();
        for index in 0..MAX_REGISTRY_PARALLELISM {
            let ready = root.join(format!("slot-{index}.ready"));
            let mut holder = spawn_lock_helper("slot", &cache_dir, &ready);
            wait_for_helper(&mut holder, &ready);
            holders.push(holder);
        }

        let plan = slot_test_plan(&cache_dir);
        let err = match acquire_registry_download_slot(
            &plan,
            Span::default(),
            Instant::now() + Duration::from_millis(50),
        ) {
            Ok(_) => panic!("a ninth process must stop at the shared slot deadline"),
            Err(err) => err,
        };
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));

        holders[0].terminate();
        let recovered = acquire_registry_download_slot(
            &plan,
            Span::default(),
            Instant::now() + Duration::from_secs(2),
        )
        .expect("a killed process must release its global download slot");
        drop(recovered);
        for holder in holders.iter_mut().skip(1) {
            holder.release_and_wait();
        }
        drop(holders);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_dependency_work_slots_are_global_across_processes() {
        let root = process_test_root("cross-process-file-slots");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&cache_dir).expect("create shared file slot cache");
        let mut holders = Vec::new();
        for index in 0..MAX_REGISTRY_PARALLELISM {
            let ready = root.join(format!("file-slot-{index}.ready"));
            let mut holder = spawn_lock_helper("file-slot", &cache_dir, &ready);
            wait_for_helper(&mut holder, &ready);
            holders.push(holder);
        }

        let err = match acquire_file_work_slot(
            &cache_dir,
            Span::default(),
            Instant::now() + Duration::from_millis(50),
        ) {
            Ok(_) => panic!("a ninth process must stop at the shared file slot deadline"),
            Err(err) => err,
        };
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));

        holders[0].terminate();
        let recovered = acquire_file_work_slot(
            &cache_dir,
            Span::default(),
            Instant::now() + Duration::from_secs(2),
        )
        .expect("a killed process must release its global file dependency slot");
        drop(recovered);
        for holder in holders.iter_mut().skip(1) {
            holder.release_and_wait();
        }
        drop(holders);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_usage_lease_blocks_cross_process_gc_and_releases_after_termination() {
        let root = process_test_root("cross-process-usage-lock");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&cache_dir).expect("create shared package cache");
        let ready = root.join("reader.ready");
        let mut reader = spawn_lock_helper("usage", &cache_dir, &ready);
        wait_for_helper(&mut reader, &ready);

        let usage_lock = cache_dir.join(".package-usage.lock");
        let err = match acquire_registry_install_lock_until(
            &usage_lock,
            Span::default(),
            Instant::now() + Duration::from_millis(50),
        ) {
            Ok(_) => panic!("GC must not take its exclusive lock while another process reads"),
            Err(err) => err,
        };
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));

        reader.terminate();
        let exclusive = acquire_registry_install_lock_until(
            &usage_lock,
            Span::default(),
            Instant::now() + Duration::from_secs(2),
        )
        .expect("the OS must release a shared usage lease after process termination");
        drop(exclusive);
        let _ = fs::remove_dir_all(root);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRegistry {
    pub url: String,
    pub public_key: String,
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
    pub resolved_file_dependencies: Vec<ResolvedFileDependency>,
    pub resolved_registry_dependencies: Vec<ResolvedPackageDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageImportScope {
    pub package_name: String,
    pub import_root: PathBuf,
    pub dependencies: Vec<PackageDependency>,
    is_consumer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackageDependency {
    pub name: String,
    pub requirement: String,
    pub version: String,
    pub registry: String,
    pub url: String,
    pub checksum: String,
    pub cache_key: String,
    pub package_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFileDependency {
    pub name: String,
    pub requirement: String,
    pub version: String,
    pub source: String,
    pub checksum: String,
    pub cache_key: String,
    pub package_root: PathBuf,
    pub dependencies: Vec<PackageDependency>,
    pub snapshot_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageArtifact {
    pub path: PathBuf,
    pub name: String,
    pub version: String,
    pub checksum: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagePublishReceipt {
    pub name: String,
    pub version: String,
    pub checksum: String,
    pub registry: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageYankReceipt {
    pub name: String,
    pub version: String,
    pub registry: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagePublishPlan {
    pub url: String,
    pub artifact_path: PathBuf,
    pub checksum: String,
    pub content_length: u64,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockDependency {
    pub path: PathBuf,
    pub cache_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryManifest {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) source: String,
    pub(crate) checksum: String,
    pub(crate) dependencies: Vec<PackageDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryIndex {
    pub(crate) name: String,
    pub(crate) versions: Vec<RegistryManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Ed25519RegistryIndexVerifier {
    public_key: [u8; 32],
    signature: [u8; 64],
}

impl Ed25519RegistryIndexVerifier {
    fn new(public_key: [u8; 32], signature: [u8; 64]) -> Self {
        Self {
            public_key,
            signature,
        }
    }

    fn verify(&self, index_url: &str, index_bytes: &[u8], span: Span) -> KuResult<()> {
        let public_key = VerifyingKey::from_bytes(&self.public_key).map_err(|_| {
            KuError::package(
                "invalid_registry_public_key",
                format!("registry index '{index_url}' has an invalid Ed25519 public key"),
                span,
            )
        })?;
        let signature = Signature::from_bytes(&self.signature);
        public_key.verify(index_bytes, &signature).map_err(|_| {
            KuError::package(
                "registry_signature_mismatch",
                format!(
                    "registry index '{index_url}' failed Ed25519 detached signature verification"
                ),
                span,
            )
        })
    }
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
pub(crate) struct RegistryFetchPolicy {
    pub(crate) max_attempts: u8,
    pub(crate) connect_timeout_ms: u64,
    pub(crate) read_timeout_ms: u64,
    pub(crate) max_download_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageArchivePolicy {
    pub max_compressed_bytes: u64,
    pub max_unpacked_bytes: u64,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_path_bytes: usize,
    pub max_depth: usize,
}

impl Default for RegistryFetchPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            connect_timeout_ms: 10_000,
            read_timeout_ms: 30_000,
            max_download_bytes: MAX_REGISTRY_DOWNLOAD_BYTES,
        }
    }
}

impl Default for PackageArchivePolicy {
    fn default() -> Self {
        Self {
            max_compressed_bytes: MAX_REGISTRY_DOWNLOAD_BYTES,
            max_unpacked_bytes: 128_000_000,
            max_files: 4096,
            max_file_bytes: 16_000_000,
            max_path_bytes: 240,
            max_depth: 32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryCacheAction {
    ReuseVerified,
    DownloadAndReplace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyResolveMode {
    Refresh,
    Update,
    Locked,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryNetworkPolicy {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryDownloadPlan {
    pub(crate) cache_dir: PathBuf,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) url: String,
    pub(crate) checksum: String,
    pub(crate) target_dir: PathBuf,
    pub(crate) temporary_dir: PathBuf,
    pub(crate) action: RegistryCacheAction,
    pub(crate) policy: RegistryFetchPolicy,
}

pub const MANIFEST_FILE: &str = "ku.mod";
pub const LOCK_FILE: &str = "ku.lock";
pub const DEFAULT_IMPORT_ROOT: &str = "src";
pub const DEFAULT_MAIN_FILE: &str = "main.ku";
pub const DEFAULT_BUILD_DIR: &str = ".ku/build";
pub const DEFAULT_CACHE_DIR: &str = ".ku/cache";
const PACKAGE_CACHE_DIR: &str = "packages";
const PACKAGE_LOCKS_DIR: &str = ".package-locks";
const MAX_PACKAGE_BYTES: u64 = 10_000_000;
const MAX_PACKAGE_FILES: usize = 512;
const MAX_PACKAGE_DEPTH: usize = 32;
// 2^27 bytes is the largest decompression window accepted for package archives.
// This matches the default unpacked-byte budget while preventing a small frame
// header from reserving an attacker-selected zstd window.
const MAX_PACKAGE_ZSTD_WINDOW_LOG: u32 = 27;
const MAX_MANIFEST_BYTES: u64 = 256_000;
const MAX_PACKAGE_LOCK_BYTES: u64 = 20_000_000;
const MAX_LOCKED_DEPENDENCY_BYTES: u64 = 1_000_000;
const MAX_LOCK_DEPENDENCIES: usize = 4_096;
const MAX_LOCK_DEPENDENCY_PATH_BYTES: usize = 4_096;
const MAX_REGISTRY_FETCH_ATTEMPTS: u8 = 8;
pub(crate) const MAX_REGISTRY_DOWNLOAD_BYTES: u64 = 32_000_000;
const MAX_REGISTRY_TIMEOUT_MS: u64 = 300_000;
const MAX_REGISTRY_TOTAL_TIMEOUT_MS: u64 = 150_000;
const MAX_REGISTRY_RESOLVE_SECS: u64 = 300;
const REGISTRY_ARTIFACT_FILE: &str = "package.archive";
const REGISTRY_UNPACKED_DIR: &str = "package";
const REGISTRY_CHECKSUM_FILE: &str = ".sha256";
const REGISTRY_TREE_CHECKSUM_FILE: &str = ".tree-sha256";
const REGISTRY_QUARANTINE_PREFIX: &str = ".registry-quarantine-";
const MAX_REGISTRY_QUARANTINES_PER_PACKAGE: usize = 4;
const MAX_REGISTRY_QUARANTINE_SCAN_ENTRIES: usize = 4_096;
const REGISTRY_INSTALL_LOCK_ATTEMPTS: u16 = 1_800;
const REGISTRY_INSTALL_LOCK_DELAY_MS: u64 = 100;
const REGISTRY_STAGING_STALE_SECS: u64 = 24 * 60 * 60;
const MAX_PACK_STAGING_SCAN_ENTRIES: usize = 256;
const MAX_PACK_STAGING_NESTED_ENTRIES: usize = 4_096;
const MAX_LOCK_STAGING_SCAN_ENTRIES: usize = 128;
const MIN_CACHE_GC_SCAN_ENTRIES: usize = 512;
const MAX_CACHE_GC_SCAN_ENTRIES: usize = 65_536;
const CACHE_GC_SCAN_MULTIPLIER: usize = 64;
const CACHE_GC_DEADLINE_SECS: u64 = 5;
pub(crate) const MAX_REGISTRY_INDEX_BYTES: u64 = 1_000_000;
const MAX_REGISTRY_SIGNATURE_BYTES: u64 = 512;
pub(crate) const MAX_REGISTRY_INDEX_VERSIONS: usize = 4_096;
pub(crate) const MAX_REGISTRY_LINE_BYTES: usize = 8_192;
const MAX_REGISTRY_URL_BYTES: usize = 2_048;
pub(crate) const MAX_REGISTRY_DEPENDENCIES: usize = 256;
const MAX_REGISTRY_PARALLELISM: usize = 8;
const MAX_REGISTRY_PUBLISH_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_REGISTRY_SOLVER_STEPS: usize = 20_000;
const MAX_REGISTRY_GRAPH_INDEX_BYTES: u64 = 16_000_000;
const MAX_REGISTRY_GRAPH_INDEX_VERSIONS: usize = 16_384;
const MAX_REGISTRY_GRAPH_INDEX_ITEMS: usize = 65_536;
const MAX_REGISTRY_GRAPH_DOWNLOAD_BYTES: u64 = 512_000_000;
const MAX_REGISTRY_GRAPH_UNPACKED_BYTES: u64 = 1_000_000_000;
pub(crate) const MAX_PUBLISH_TOKEN_BYTES: usize = 8_192;
const PACKAGE_DISTRIBUTION_DIR: &str = ".ku/packages";
pub(crate) const PACKAGE_CONTENT_TYPE: &str = "application/vnd.ku.package+tar.zstd";
pub const REGISTRY_TOKEN_ENV: &str = "KU_REGISTRY_TOKEN";
static NEXT_REGISTRY_DOWNLOAD_ID: AtomicU64 = AtomicU64::new(1);

fn operation_nonce() -> String {
    let counter = NEXT_REGISTRY_DOWNLOAD_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{counter}-{nanos}", std::process::id())
}

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
            let source = read_manifest_file(&manifest_path)?;
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
                resolved_file_dependencies: Vec::new(),
                resolved_registry_dependencies: Vec::new(),
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
    let dependencies = lock_dependencies(package, dependencies)?;
    write_lock_with_dependency_records(package, &dependencies)
}

pub(crate) fn write_lock_with_frozen_dependencies(
    package: &PackageContext,
    dependencies: &[LockDependency],
) -> KuResult<()> {
    let expected = normalized_lock_dependency_records(package, dependencies)?;
    let paths = expected
        .iter()
        .map(|(_, dependency)| dependency.path.clone())
        .collect::<Vec<_>>();
    let current = lock_dependencies(package, &paths).map_err(|err| {
        KuError::package(
            "source_changed",
            format!("an imported source changed before ku.lock was written: {err}"),
            Span::default(),
        )
    })?;
    let current = normalized_lock_dependency_records(package, &current)?;
    if expected.len() != current.len()
        || expected.iter().zip(&current).any(
            |((expected_path, expected), (current_path, current))| {
                expected_path != current_path || expected.cache_key != current.cache_key
            },
        )
    {
        return Err(KuError::package(
            "source_changed",
            "an imported source changed after it was parsed; ku.lock was not updated",
            Span::default(),
        ));
    }
    let dependencies = expected
        .into_iter()
        .map(|(_, dependency)| dependency)
        .collect::<Vec<_>>();
    write_lock_with_dependency_records(package, &dependencies)
}

fn write_lock_with_dependency_records(
    package: &PackageContext,
    dependencies: &[LockDependency],
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
    for (_, dependency) in normalized_lock_dependency_records(package, dependencies)? {
        let path = display_dependency_path(package, &dependency.path)?;
        append_package_lock_section(
            &mut source,
            &format!(
                "\n[[dependency]]\npath = {:?}\ncache_key = {:?}\n",
                path, dependency.cache_key
            ),
        )?;
    }
    for resolved in &package.resolved_file_dependencies {
        append_package_lock_section(&mut source, &format!(
            "\n[[package_dependency]]\nname = {:?}\nrequirement = {:?}\nversion = {:?}\nsource = {:?}\nchecksum = {:?}\ncache_key = {:?}\n",
            resolved.name,
            resolved.requirement,
            resolved.version,
            resolved.source,
            resolved.checksum,
            resolved.cache_key,
        ))?;
    }
    for resolved in &package.resolved_registry_dependencies {
        append_package_lock_section(&mut source, &format!(
            "\n[[package_dependency]]\nname = {:?}\nrequirement = {:?}\nversion = {:?}\nsource = \"registry\"\nregistry = {:?}\nurl = {:?}\nchecksum = {:?}\ncache_key = {:?}\n",
            resolved.name,
            resolved.requirement,
            resolved.version,
            resolved.registry,
            resolved.url,
            resolved.checksum,
            resolved.cache_key,
        ))?;
    }
    write_package_lock_atomically(&package.lock_path, source.as_bytes())
}

fn normalized_lock_dependency_records(
    package: &PackageContext,
    dependencies: &[LockDependency],
) -> KuResult<Vec<(String, LockDependency)>> {
    if dependencies.len() > MAX_LOCK_DEPENDENCIES {
        return Err(KuError::package(
            "lock_dependency_limit",
            format!("ku.lock cannot record more than {MAX_LOCK_DEPENDENCIES} imported modules"),
            Span::default(),
        ));
    }
    let mut output = dependencies
        .iter()
        .map(|dependency| {
            Ok((
                display_dependency_path(package, &dependency.path)?,
                dependency.clone(),
            ))
        })
        .collect::<KuResult<Vec<_>>>()?;
    output.sort_by(|left, right| left.0.cmp(&right.0));
    output.dedup_by(|left, right| left.0 == right.0);
    Ok(output)
}

fn append_package_lock_section(source: &mut String, section: &str) -> KuResult<()> {
    let next_len = source.len().saturating_add(section.len());
    if next_len as u64 > MAX_PACKAGE_LOCK_BYTES {
        return Err(KuError::package(
            "package_lock_limit",
            format!("generated ku.lock exceeds {MAX_PACKAGE_LOCK_BYTES} bytes"),
            Span::default(),
        ));
    }
    source.push_str(section);
    Ok(())
}

fn package_lock_io_path(lock_path: &Path) -> PathBuf {
    lock_path.with_extension("lock.io.lock")
}

fn write_package_lock_atomically(lock_path: &Path, source: &[u8]) -> KuResult<()> {
    if source.len() as u64 > MAX_PACKAGE_LOCK_BYTES {
        return Err(KuError::package(
            "package_lock_limit",
            format!("generated ku.lock exceeds {MAX_PACKAGE_LOCK_BYTES} bytes"),
            Span::default(),
        ));
    }
    if source
        .split(|byte| *byte == b'\n')
        .any(|line| line.len() > MAX_REGISTRY_LINE_BYTES)
    {
        return Err(KuError::package(
            "package_lock_limit",
            format!(
                "generated ku.lock contains a line longer than {MAX_REGISTRY_LINE_BYTES} bytes"
            ),
            Span::default(),
        ));
    }
    write_package_lock_atomically_with_stale_after(
        lock_path,
        source,
        Duration::from_secs(REGISTRY_STAGING_STALE_SECS),
    )
}

fn write_package_lock_atomically_with_stale_after(
    lock_path: &Path,
    source: &[u8],
    stale_after: Duration,
) -> KuResult<()> {
    let io_lock_path = package_lock_io_path(lock_path);
    let _lock = acquire_registry_install_lock(&io_lock_path, Span::default())?;
    cleanup_stale_package_lock_staging(lock_path, stale_after, MAX_LOCK_STAGING_SCAN_ENTRIES)?;
    let id = operation_nonce();
    let temporary_path = lock_path.with_extension(format!("lock.{id}.tmp"));
    let temporary = TemporaryFile::new(temporary_path.clone());
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .map_err(|err| {
            KuError::message(format!(
                "failed to create package lock staging '{}': {err}",
                temporary_path.display()
            ))
        })?;
    file.write_all(source).map_err(|err| {
        KuError::message(format!(
            "failed to write package lock staging '{}': {err}",
            temporary_path.display()
        ))
    })?;
    file.sync_all().map_err(|err| {
        KuError::message(format!(
            "failed to sync package lock staging '{}': {err}",
            temporary_path.display()
        ))
    })?;
    drop(file);
    replace_file_atomically(&temporary_path, lock_path).map_err(|err| {
        KuError::message(format!(
            "failed to atomically install package lock '{}': {err}",
            lock_path.display()
        ))
    })?;
    sync_parent_directory(lock_path)?;
    drop(temporary);
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomically(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file_atomically(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let mut source_wide = source.as_os_str().encode_wide().collect::<Vec<_>>();
    source_wide.push(0);
    let mut target_wide = target.as_os_str().encode_wide().collect::<Vec<_>>();
    target_wide.push(0);
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> KuResult<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|err| {
            KuError::message(format!(
                "failed to sync package lock directory '{}': {err}",
                parent.display()
            ))
        })
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> KuResult<()> {
    Ok(())
}

pub fn resolve_remote_dependencies(package: &mut PackageContext) -> KuResult<()> {
    resolve_remote_dependencies_with_mode(package, DependencyResolveMode::Update)
}

pub fn resolve_remote_dependencies_with_mode(
    package: &mut PackageContext,
    mode: DependencyResolveMode,
) -> KuResult<()> {
    let deadline = package_operation_deadline();
    let _usage_lease = acquire_package_usage_lease_until(package, deadline)?;
    resolve_remote_dependencies_with_mode_until(package, mode, deadline)
}

pub(crate) fn package_operation_deadline() -> Instant {
    Instant::now() + Duration::from_secs(MAX_REGISTRY_RESOLVE_SECS)
}

pub(crate) fn resolve_remote_dependencies_with_mode_until(
    package: &mut PackageContext,
    mode: DependencyResolveMode,
    deadline: Instant,
) -> KuResult<()> {
    ensure_registry_deadline(deadline, Span::default())?;
    ensure_cache_dir(package)?;
    if package.manifest.dependencies.len() > MAX_REGISTRY_DEPENDENCIES {
        return Err(KuError::package(
            "dependency_limit",
            format!("package declares more than {MAX_REGISTRY_DEPENDENCIES} dependencies"),
            Span::default(),
        ));
    }

    let dependencies = package.manifest.dependencies.clone();
    let file_dependencies = dependencies
        .iter()
        .filter(|dependency| dependency.source.is_some())
        .cloned()
        .collect::<Vec<_>>();
    let locked_file_dependencies = if matches!(
        mode,
        DependencyResolveMode::Update | DependencyResolveMode::Refresh
    ) {
        HashMap::new()
    } else {
        read_locked_file_dependencies(package)?
    };
    let mut resolved_file_dependencies = Vec::with_capacity(file_dependencies.len());
    let mut graph_budget = RegistryGraphResourceBudget::default();
    let mut file_offset = 0usize;
    while file_offset < file_dependencies.len() {
        let chunk_len = MAX_REGISTRY_PARALLELISM
            .min(graph_budget.available_file_work_slots())
            .min(file_dependencies.len() - file_offset);
        graph_budget.require_file_work_slots(chunk_len)?;
        let chunk = &file_dependencies[file_offset..file_offset + chunk_len];
        let chunk_results = thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|dependency| {
                    scope.spawn(|| {
                        let _work_slot =
                            acquire_file_work_slot(&package.cache_dir, Span::default(), deadline)?;
                        resolve_file_dependency_with_mode(
                            package,
                            dependency,
                            dependency
                                .source
                                .as_deref()
                                .expect("filtered file dependency has source"),
                            mode,
                            locked_file_dependencies.get(&dependency.name),
                            deadline,
                        )
                    })
                })
                .collect::<Vec<_>>();
            let mut output = Vec::with_capacity(handles.len());
            for handle in handles {
                output.push(handle.join().map_err(|_| {
                    KuError::package(
                        "dependency_worker_failed",
                        "package dependency worker terminated unexpectedly",
                        Span::default(),
                    )
                })??);
            }
            Ok::<_, KuError>(output)
        })?;
        for dependency in &chunk_results {
            graph_budget.record_file_snapshot(dependency.snapshot_bytes)?;
        }
        resolved_file_dependencies.extend(chunk_results);
        file_offset += chunk_len;
        ensure_registry_deadline(deadline, Span::default())?;
    }

    resolved_file_dependencies.sort_by(|left, right| left.name.cmp(&right.name));
    let file_overrides = file_override_map(&resolved_file_dependencies)?;
    let graph_roots = dependencies
        .into_iter()
        .map(|mut dependency| {
            dependency.source = None;
            dependency.checksum = None;
            dependency
        })
        .collect::<Vec<_>>();
    let initial_constraints =
        collect_active_registry_constraints(&graph_roots, &HashMap::new(), &file_overrides)?;
    let requires_registry = initial_constraints
        .keys()
        .any(|name| !file_overrides.contains_key(name));
    if !requires_registry {
        package.resolved_file_dependencies = resolved_file_dependencies;
        package.resolved_registry_dependencies.clear();
        return Ok(());
    }
    let registry = package.manifest.registry.clone().ok_or_else(|| {
        KuError::package(
            "registry_trust_unconfigured",
            "registry dependencies require a trusted source configured with registry.url and registry.public_key in ku.mod",
            Span::default(),
        )
    })?;
    let session_policy = registry_fetch_policy_for_deadline(
        deadline,
        RegistryFetchPolicy::default().max_download_bytes,
        Span::default(),
    )?;
    let http_client = RegistryHttpClient::new(session_policy, &registry.url);
    if mode != DependencyResolveMode::Refresh {
        let locked = read_locked_registry_dependencies(package)?;
        if let Some(mut resolved) = try_resolve_locked_registry_graph(
            package,
            &registry,
            &graph_roots,
            &file_overrides,
            &locked,
            mode == DependencyResolveMode::Offline,
            deadline,
            &mut graph_budget,
            &http_client,
        )? {
            resolved.sort_by(|left, right| left.name.cmp(&right.name));
            package.resolved_file_dependencies = resolved_file_dependencies;
            package.resolved_registry_dependencies = resolved;
            return Ok(());
        }
    }
    if matches!(
        mode,
        DependencyResolveMode::Locked | DependencyResolveMode::Offline
    ) {
        return Err(KuError::package(
            "lock_out_of_date",
            "ku.lock does not contain a complete registry graph satisfying ku.mod",
            Span::default(),
        ));
    }

    let mut solver = RegistryGraphSolver::with_deadline_overrides_and_http_client(
        &registry,
        deadline,
        file_overrides.clone(),
        http_client.clone(),
    );
    let selected = solver.solve(&graph_roots)?;
    let constraints =
        collect_active_registry_constraints(&graph_roots, &selected, &file_overrides)?;
    let mut manifests = selected.into_values().collect::<Vec<_>>();
    manifests.sort_by(|left, right| left.name.cmp(&right.name));
    let mut resolved = Vec::with_capacity(manifests.len());
    let package_ref: &PackageContext = package;
    let registry_ref = &registry;
    let http_client_ref = &http_client;
    let mut offset = 0usize;
    while offset < manifests.len() {
        let chunk_len = MAX_REGISTRY_PARALLELISM
            .min(graph_budget.available_install_slots())
            .min(manifests.len() - offset);
        graph_budget.require_install_slots(chunk_len)?;
        let chunk = &manifests[offset..offset + chunk_len];
        let chunk_results = thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|manifest| {
                    let requirements = constraints.get(&manifest.name).cloned().unwrap_or_default();
                    scope.spawn(move || {
                        install_registry_manifest(
                            package_ref,
                            registry_ref,
                            manifest,
                            &requirements,
                            deadline,
                            http_client_ref,
                        )
                    })
                })
                .collect::<Vec<_>>();
            let mut output = Vec::with_capacity(handles.len());
            for handle in handles {
                output.push(handle.join().map_err(|_| {
                    KuError::package(
                        "dependency_worker_failed",
                        "package dependency worker terminated unexpectedly",
                        Span::default(),
                    )
                })??);
            }
            Ok::<_, KuError>(output)
        })?;
        for installed in chunk_results {
            graph_budget.record_registry_usage(installed.usage)?;
            resolved.push(installed.dependency);
        }
        offset += chunk_len;
    }
    resolved.sort_by(|left, right| left.name.cmp(&right.name));
    package.resolved_file_dependencies = resolved_file_dependencies;
    package.resolved_registry_dependencies = resolved;
    Ok(())
}

#[derive(Debug, Clone)]
struct LockedRegistryDependency {
    name: String,
    version: String,
    registry: String,
    url: String,
    checksum: String,
    cache_key: String,
}

#[derive(Debug, Clone)]
struct LockedFileDependency {
    name: String,
    requirement: String,
    version: String,
    source: String,
    checksum: String,
    cache_key: String,
}

#[derive(Debug, Clone)]
struct FileOverride {
    version: String,
    dependencies: Vec<PackageDependency>,
}

#[derive(Debug, Clone, Copy)]
struct RegistryIndexResourceUsage {
    source_bytes: u64,
    versions: usize,
    parsed_items: usize,
}

struct CachedRegistryIndex {
    index: RegistryIndex,
    usage: RegistryIndexResourceUsage,
}

#[derive(Default)]
struct RegistryIndexGraphBudget {
    source_bytes: u64,
    versions: usize,
    parsed_items: usize,
}

impl RegistryIndexResourceUsage {
    fn from_index(index: &RegistryIndex, source_bytes: u64) -> KuResult<Self> {
        let dependencies = index.versions.iter().try_fold(0usize, |total, version| {
            total
                .checked_add(version.dependencies.len())
                .ok_or_else(|| {
                    registry_index_graph_limit_error("signed registry index item count overflowed")
                })
        })?;
        let parsed_items = 1usize
            .checked_add(index.versions.len())
            .and_then(|total| total.checked_add(dependencies))
            .ok_or_else(|| {
                registry_index_graph_limit_error("signed registry index item count overflowed")
            })?;
        Ok(Self {
            source_bytes,
            versions: index.versions.len(),
            parsed_items,
        })
    }
}

impl RegistryIndexGraphBudget {
    fn record(&mut self, usage: RegistryIndexResourceUsage) -> KuResult<()> {
        let source_bytes = self
            .source_bytes
            .checked_add(usage.source_bytes)
            .ok_or_else(|| {
                registry_index_graph_limit_error("signed registry index byte count overflowed")
            })?;
        let versions = self.versions.checked_add(usage.versions).ok_or_else(|| {
            registry_index_graph_limit_error("signed registry index version count overflowed")
        })?;
        let parsed_items = self
            .parsed_items
            .checked_add(usage.parsed_items)
            .ok_or_else(|| {
                registry_index_graph_limit_error("signed registry index item count overflowed")
            })?;
        if source_bytes > MAX_REGISTRY_GRAPH_INDEX_BYTES
            || versions > MAX_REGISTRY_GRAPH_INDEX_VERSIONS
            || parsed_items > MAX_REGISTRY_GRAPH_INDEX_ITEMS
        {
            return Err(registry_index_graph_limit_error(format!(
                "signed registry dependency graph exceeds index limits: {source_bytes}/{MAX_REGISTRY_GRAPH_INDEX_BYTES} bytes, {versions}/{MAX_REGISTRY_GRAPH_INDEX_VERSIONS} versions, {parsed_items}/{MAX_REGISTRY_GRAPH_INDEX_ITEMS} parsed items"
            )));
        }
        self.source_bytes = source_bytes;
        self.versions = versions;
        self.parsed_items = parsed_items;
        Ok(())
    }
}

fn registry_index_graph_limit_error(message: impl Into<String>) -> KuError {
    KuError::package(
        "registry_index_graph_limit",
        message.into(),
        Span::default(),
    )
}

struct RegistryGraphSolver<'a> {
    registry: &'a PackageRegistry,
    http_client: RegistryHttpClient,
    file_overrides: HashMap<String, FileOverride>,
    indexes: HashMap<String, CachedRegistryIndex>,
    index_budget: RegistryIndexGraphBudget,
    steps: usize,
    deadline: Instant,
}

impl<'a> RegistryGraphSolver<'a> {
    #[cfg(test)]
    fn new(registry: &'a PackageRegistry) -> Self {
        Self::with_deadline(
            registry,
            Instant::now() + Duration::from_secs(MAX_REGISTRY_RESOLVE_SECS),
        )
    }

    #[cfg(test)]
    fn with_deadline(registry: &'a PackageRegistry, deadline: Instant) -> Self {
        Self::with_deadline_and_overrides(registry, deadline, HashMap::new())
    }

    #[cfg(test)]
    fn with_deadline_and_overrides(
        registry: &'a PackageRegistry,
        deadline: Instant,
        file_overrides: HashMap<String, FileOverride>,
    ) -> Self {
        Self::with_deadline_overrides_and_http_client(
            registry,
            deadline,
            file_overrides,
            RegistryHttpClient::new(RegistryFetchPolicy::default(), &registry.url),
        )
    }

    fn with_deadline_overrides_and_http_client(
        registry: &'a PackageRegistry,
        deadline: Instant,
        file_overrides: HashMap<String, FileOverride>,
        http_client: RegistryHttpClient,
    ) -> Self {
        Self {
            registry,
            http_client,
            file_overrides,
            indexes: HashMap::new(),
            index_budget: RegistryIndexGraphBudget::default(),
            steps: 0,
            deadline,
        }
    }

    fn solve(
        &mut self,
        roots: &[PackageDependency],
    ) -> KuResult<HashMap<String, RegistryManifest>> {
        let mut assignments = HashMap::new();
        if self.search(roots, &mut assignments)? {
            Ok(assignments)
        } else {
            Err(KuError::package(
                "dependency_conflict",
                "registry dependency graph has no version set satisfying all exact and caret requirements",
                Span::default(),
            ))
        }
    }

    fn search(
        &mut self,
        roots: &[PackageDependency],
        assignments: &mut HashMap<String, RegistryManifest>,
    ) -> KuResult<bool> {
        ensure_registry_deadline(self.deadline, Span::default())?;
        self.steps = self.steps.saturating_add(1);
        if self.steps > MAX_REGISTRY_SOLVER_STEPS {
            return Err(KuError::package(
                "resolver_limit",
                format!(
                    "registry dependency solver exceeded {MAX_REGISTRY_SOLVER_STEPS} bounded steps"
                ),
                Span::default(),
            ));
        }
        let constraints =
            collect_active_registry_constraints(roots, assignments, &self.file_overrides)?;
        let mut names = constraints.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in &names {
            if let Some(file_override) = self.file_overrides.get(name) {
                if !registry_version_satisfies_all(
                    &file_override.version,
                    &constraints[name],
                    Span::default(),
                )? {
                    return Ok(false);
                }
                continue;
            }
            if let Some(assigned) = assignments.get(name) {
                if !registry_version_satisfies_all(
                    &assigned.version,
                    &constraints[name],
                    Span::default(),
                )? {
                    return Ok(false);
                }
            }
        }
        let Some(name) = names.into_iter().find(|name| {
            !assignments.contains_key(name) && !self.file_overrides.contains_key(name)
        }) else {
            return Ok(true);
        };
        let requirements = &constraints[&name];
        let cached = self.take_registry_index(&name)?;
        let result = (|| {
            for candidate in &cached.index.versions {
                if !registry_version_satisfies_all(
                    &candidate.version,
                    requirements,
                    Span::default(),
                )? {
                    continue;
                }
                self.steps = self.steps.saturating_add(1);
                if self.steps > MAX_REGISTRY_SOLVER_STEPS {
                    return Err(KuError::package(
                        "resolver_limit",
                        format!(
                            "registry dependency solver exceeded {MAX_REGISTRY_SOLVER_STEPS} bounded steps"
                        ),
                        Span::default(),
                    ));
                }
                assignments.insert(name.clone(), candidate.clone());
                match self.search(roots, assignments) {
                    Ok(true) => return Ok(true),
                    Ok(false) => {
                        assignments.remove(&name);
                    }
                    Err(err) => {
                        assignments.remove(&name);
                        return Err(err);
                    }
                }
            }
            Ok(false)
        })();
        self.indexes.insert(name, cached);
        result
    }

    fn take_registry_index(&mut self, name: &str) -> KuResult<CachedRegistryIndex> {
        if let Some(index) = self.indexes.remove(name) {
            return Ok(index);
        }
        let index = fetch_trusted_registry_index_with_usage_until(
            self.registry,
            name,
            Span::default(),
            self.deadline,
            &self.http_client,
        )?;
        if index.index.name != name {
            return Err(KuError::package(
                "registry_name_mismatch",
                format!(
                    "registry returned package '{}' for requested dependency '{name}'",
                    index.index.name
                ),
                Span::default(),
            ));
        }
        self.index_budget.record(index.usage)?;
        Ok(index)
    }

    #[cfg(test)]
    fn insert_test_index(&mut self, index: RegistryIndex) -> KuResult<()> {
        self.insert_test_index_with_source_bytes(index, 0)
    }

    #[cfg(test)]
    fn insert_test_index_with_source_bytes(
        &mut self,
        index: RegistryIndex,
        source_bytes: u64,
    ) -> KuResult<()> {
        let usage = RegistryIndexResourceUsage::from_index(&index, source_bytes)?;
        self.index_budget.record(usage)?;
        self.indexes
            .insert(index.name.clone(), CachedRegistryIndex { index, usage });
        Ok(())
    }
}

fn collect_active_registry_constraints(
    roots: &[PackageDependency],
    assignments: &HashMap<String, RegistryManifest>,
    file_overrides: &HashMap<String, FileOverride>,
) -> KuResult<HashMap<String, Vec<String>>> {
    let mut constraints = HashMap::<String, Vec<String>>::new();
    let mut queue = Vec::new();
    for dependency in roots {
        add_registry_constraint(&mut constraints, &mut queue, dependency)?;
    }
    let mut expanded = HashSet::new();
    while let Some(name) = queue.pop() {
        if constraints.len() > MAX_REGISTRY_DEPENDENCIES {
            return Err(KuError::package(
                "dependency_limit",
                format!("registry dependency graph exceeds {MAX_REGISTRY_DEPENDENCIES} packages"),
                Span::default(),
            ));
        }
        if !expanded.insert(name.clone()) {
            continue;
        }
        if let Some(file_override) = file_overrides.get(&name) {
            for dependency in &file_override.dependencies {
                add_registry_constraint(&mut constraints, &mut queue, dependency)?;
            }
        } else if let Some(manifest) = assignments.get(&name) {
            for dependency in &manifest.dependencies {
                add_registry_constraint(&mut constraints, &mut queue, dependency)?;
            }
        }
    }
    Ok(constraints)
}

fn file_override_map(
    dependencies: &[ResolvedFileDependency],
) -> KuResult<HashMap<String, FileOverride>> {
    let mut output = HashMap::with_capacity(dependencies.len());
    for dependency in dependencies {
        let file_override = FileOverride {
            version: dependency.version.clone(),
            dependencies: dependency.dependencies.clone(),
        };
        if output
            .insert(dependency.name.clone(), file_override)
            .is_some()
        {
            return Err(KuError::package(
                "duplicate_package_name",
                format!(
                    "file dependency '{}' is resolved more than once",
                    dependency.name
                ),
                Span::default(),
            ));
        }
    }
    Ok(output)
}

fn add_registry_constraint(
    constraints: &mut HashMap<String, Vec<String>>,
    queue: &mut Vec<String>,
    dependency: &PackageDependency,
) -> KuResult<()> {
    if dependency.source.is_some() || dependency.checksum.is_some() {
        return Err(KuError::package(
            "invalid_registry_dependency",
            format!(
                "published dependency '{}' must use dep.name = \"version\" without source or checksum",
                dependency.name
            ),
            Span::default(),
        ));
    }
    validate_package_name(&dependency.name, Span::default())?;
    validate_version_requirement(&dependency.version, Span::default())?;
    let requirements = constraints.entry(dependency.name.clone()).or_default();
    if !requirements.contains(&dependency.version) {
        requirements.push(dependency.version.clone());
        requirements.sort();
    }
    queue.push(dependency.name.clone());
    Ok(())
}

fn registry_version_satisfies_all(
    version: &str,
    requirements: &[String],
    span: Span,
) -> KuResult<bool> {
    let version = parse_package_version(version, span)?;
    for requirement in requirements {
        let requirement = parse_version_requirement(requirement, span)?;
        if !version_requirement_matches(requirement, version) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn try_resolve_locked_registry_graph(
    package: &PackageContext,
    registry: &PackageRegistry,
    roots: &[PackageDependency],
    file_overrides: &HashMap<String, FileOverride>,
    locked: &HashMap<String, LockedRegistryDependency>,
    offline: bool,
    deadline: Instant,
    graph_budget: &mut RegistryGraphResourceBudget,
    http_client: &RegistryHttpClient,
) -> KuResult<Option<Vec<ResolvedPackageDependency>>> {
    let mut assignments = HashMap::<String, RegistryManifest>::new();
    let mut package_roots = HashMap::<String, ResolvedRegistryCache>::new();
    loop {
        ensure_registry_deadline(deadline, Span::default())?;
        let constraints = collect_active_registry_constraints(roots, &assignments, file_overrides)?;
        let mut names = constraints.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in &names {
            if let Some(file_override) = file_overrides.get(name) {
                if !registry_version_satisfies_all(
                    &file_override.version,
                    &constraints[name],
                    Span::default(),
                )? {
                    return Ok(None);
                }
                continue;
            }
            if let Some(assigned) = assignments.get(name) {
                if !registry_version_satisfies_all(
                    &assigned.version,
                    &constraints[name],
                    Span::default(),
                )? {
                    return Ok(None);
                }
            }
        }
        let Some(name) = names
            .into_iter()
            .find(|name| !assignments.contains_key(name) && !file_overrides.contains_key(name))
        else {
            let mut resolved = Vec::with_capacity(assignments.len());
            for (name, manifest) in assignments {
                let locked = locked
                    .get(&name)
                    .expect("locked graph assignment came from lock");
                let resolved_cache = package_roots
                    .remove(&name)
                    .expect("locked package root was recorded");
                resolved.push(ResolvedPackageDependency {
                    name: name.clone(),
                    requirement: constraints[&name].join(" && "),
                    version: manifest.version,
                    registry: registry.url.clone(),
                    url: manifest.source,
                    checksum: manifest.checksum,
                    cache_key: locked.cache_key.clone(),
                    package_root: resolved_cache.package_root,
                });
            }
            return Ok(Some(resolved));
        };
        let Some(locked) = locked.get(&name) else {
            return Ok(None);
        };
        if locked.registry != registry.url
            || !registry_version_satisfies_all(
                &locked.version,
                &constraints[&name],
                Span::default(),
            )?
        {
            return Ok(None);
        }
        let locked_manifest = RegistryManifest {
            name: locked.name.clone(),
            version: locked.version.clone(),
            source: locked.url.clone(),
            checksum: locked.checksum.clone(),
            dependencies: Vec::new(),
        };
        let policy = registry_fetch_policy_for_deadline(
            deadline,
            RegistryFetchPolicy::default().max_download_bytes,
            Span::default(),
        )?;
        let plan = plan_registry_download(
            &package.cache_dir,
            &locked_manifest,
            Some(&locked.checksum),
            policy,
            Span::default(),
        )?;
        if plan.target_dir.file_name().and_then(|value| value.to_str())
            != Some(locked.cache_key.as_str())
        {
            return Err(KuError::package(
                "invalid_cache_key",
                format!("ku.lock has an invalid cache key for '{name}'"),
                Span::default(),
            ));
        }
        graph_budget.require_install_slots(1)?;
        let network_policy = if offline {
            RegistryNetworkPolicy::Deny
        } else {
            RegistryNetworkPolicy::Allow
        };
        let resolved_cache = execute_registry_download_with_usage_until(
            &plan,
            Span::default(),
            deadline,
            network_policy,
            http_client,
        )?;
        graph_budget.record_registry_usage(resolved_cache.usage)?;
        let package_root = resolved_cache.package_root.clone();
        let package_manifest =
            validate_package_root_identity(&package_root, &name, &locked.version, Span::default())?;
        validate_registry_package_dependencies(&package_manifest.dependencies)?;
        assignments.insert(
            name.clone(),
            RegistryManifest {
                dependencies: package_manifest.dependencies,
                ..locked_manifest
            },
        );
        package_roots.insert(name, resolved_cache);
    }
}

struct InstalledRegistryDependency {
    dependency: ResolvedPackageDependency,
    usage: RegistryPackageResourceUsage,
}

fn install_registry_manifest(
    package: &PackageContext,
    registry: &PackageRegistry,
    manifest: &RegistryManifest,
    requirements: &[String],
    deadline: Instant,
    http_client: &RegistryHttpClient,
) -> KuResult<InstalledRegistryDependency> {
    let policy = registry_fetch_policy_for_deadline(
        deadline,
        RegistryFetchPolicy::default().max_download_bytes,
        Span::default(),
    )?;
    let plan = plan_registry_download(&package.cache_dir, manifest, None, policy, Span::default())?;
    let cache_key = plan
        .target_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            KuError::package(
                "invalid_cache_path",
                "registry cache target has no valid UTF-8 file name",
                Span::default(),
            )
        })?
        .to_string();
    let resolved_cache = execute_registry_download_with_usage_until(
        &plan,
        Span::default(),
        deadline,
        RegistryNetworkPolicy::Allow,
        http_client,
    )?;
    let package_root = resolved_cache.package_root;
    let package_manifest = validate_package_root_identity(
        &package_root,
        &manifest.name,
        &manifest.version,
        Span::default(),
    )?;
    validate_registry_package_dependencies(&package_manifest.dependencies)?;
    if normalized_registry_dependencies(&package_manifest.dependencies)
        != normalized_registry_dependencies(&manifest.dependencies)
    {
        return Err(KuError::package(
            "registry_dependency_mismatch",
            format!(
                "package '{}@{}' dependencies do not match its signed registry index",
                manifest.name, manifest.version
            ),
            Span::default(),
        ));
    }
    Ok(InstalledRegistryDependency {
        dependency: ResolvedPackageDependency {
            name: manifest.name.clone(),
            requirement: requirements.join(" && "),
            version: manifest.version.clone(),
            registry: registry.url.clone(),
            url: manifest.source.clone(),
            checksum: manifest.checksum.clone(),
            cache_key,
            package_root,
        },
        usage: resolved_cache.usage,
    })
}

pub(crate) fn validate_registry_package_dependencies(
    dependencies: &[PackageDependency],
) -> KuResult<()> {
    if dependencies.len() > MAX_REGISTRY_DEPENDENCIES {
        return Err(KuError::package(
            "dependency_limit",
            format!(
                "published package declares more than {MAX_REGISTRY_DEPENDENCIES} dependencies"
            ),
            Span::default(),
        ));
    }
    for dependency in dependencies {
        if dependency.source.is_some() || dependency.checksum.is_some() {
            return Err(KuError::package(
                "invalid_registry_dependency",
                format!(
                    "published package dependency '{}' cannot use a local source or checksum",
                    dependency.name
                ),
                Span::default(),
            ));
        }
    }
    Ok(())
}

fn normalized_registry_dependencies(dependencies: &[PackageDependency]) -> Vec<(String, String)> {
    let mut output = dependencies
        .iter()
        .map(|dependency| (dependency.name.clone(), dependency.version.clone()))
        .collect::<Vec<_>>();
    output.sort();
    output
}

#[derive(Default, Clone, Copy)]
struct RegistryGraphResourceBudget {
    compressed: u64,
    unpacked: u64,
}

impl RegistryGraphResourceBudget {
    fn available_file_work_slots(&self) -> usize {
        let compressed_slots =
            MAX_REGISTRY_GRAPH_DOWNLOAD_BYTES.saturating_sub(self.compressed) / MAX_PACKAGE_BYTES;
        let unpacked_slots =
            MAX_REGISTRY_GRAPH_UNPACKED_BYTES.saturating_sub(self.unpacked) / MAX_PACKAGE_BYTES;
        compressed_slots.min(unpacked_slots) as usize
    }

    fn require_file_work_slots(&self, count: usize) -> KuResult<()> {
        if count == 0 || count > self.available_file_work_slots() {
            return Err(KuError::package(
                "dependency_graph_size_limit",
                "remaining dependency graph budget cannot safely admit another file snapshot",
                Span::default(),
            ));
        }
        Ok(())
    }

    fn record_file_snapshot(&mut self, bytes: u64) -> KuResult<()> {
        if bytes > MAX_PACKAGE_BYTES {
            return Err(KuError::package(
                "dependency_graph_size_limit",
                "file dependency snapshot exceeds its per-package byte limit",
                Span::default(),
            ));
        }
        self.record(bytes, bytes, "file dependency graph")
    }

    fn available_install_slots(&self) -> usize {
        let compressed_slots = MAX_REGISTRY_GRAPH_DOWNLOAD_BYTES.saturating_sub(self.compressed)
            / MAX_REGISTRY_DOWNLOAD_BYTES;
        let unpacked_slots = MAX_REGISTRY_GRAPH_UNPACKED_BYTES.saturating_sub(self.unpacked)
            / PackageArchivePolicy::default().max_unpacked_bytes;
        compressed_slots.min(unpacked_slots) as usize
    }

    fn require_install_slots(&self, count: usize) -> KuResult<()> {
        if count == 0 || count > self.available_install_slots() {
            return Err(KuError::package(
                "dependency_graph_size_limit",
                "remaining dependency graph budget cannot safely admit another package",
                Span::default(),
            ));
        }
        Ok(())
    }

    fn record_registry_usage(&mut self, usage: RegistryPackageResourceUsage) -> KuResult<()> {
        if usage.compressed > MAX_REGISTRY_DOWNLOAD_BYTES
            || usage.unpacked > PackageArchivePolicy::default().max_unpacked_bytes
        {
            return Err(KuError::package(
                "dependency_graph_size_limit",
                "registry package usage exceeds its per-package resource limit",
                Span::default(),
            ));
        }
        self.record(
            usage.compressed,
            usage.unpacked,
            "resolved dependency graph",
        )
    }

    fn record(&mut self, compressed: u64, unpacked: u64, subject: &str) -> KuResult<()> {
        self.compressed = self.compressed.saturating_add(compressed);
        self.unpacked = self.unpacked.saturating_add(unpacked);
        if self.compressed > MAX_REGISTRY_GRAPH_DOWNLOAD_BYTES
            || self.unpacked > MAX_REGISTRY_GRAPH_UNPACKED_BYTES
        {
            return Err(KuError::package(
                "dependency_graph_size_limit",
                format!(
                    "{subject} exceeds {MAX_REGISTRY_GRAPH_DOWNLOAD_BYTES} compressed-equivalent or {MAX_REGISTRY_GRAPH_UNPACKED_BYTES} unpacked-equivalent bytes"
                ),
                Span::default(),
            ));
        }
        Ok(())
    }
}

fn validate_file_dependency_source_is_external(
    package: &PackageContext,
    source_path: &Path,
) -> KuResult<()> {
    let canonical_cache = fs::canonicalize(&package.cache_dir).map_err(|err| {
        KuError::message(format!(
            "failed to resolve package cache '{}': {err}",
            package.cache_dir.display()
        ))
    })?;
    if canonical_cache.starts_with(source_path) || source_path.starts_with(&canonical_cache) {
        return Err(KuError::package(
            "unsafe_file_dependency_source",
            "file dependency source must not contain or be contained by the consumer cache",
            Span::default(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct FileDependencyIdentity {
    version: String,
    dependencies: Vec<PackageDependency>,
}

fn inspect_file_dependency_identity(
    source_path: &Path,
    dependency: &PackageDependency,
    span: Span,
) -> KuResult<FileDependencyIdentity> {
    let manifest_path = source_path.join(MANIFEST_FILE);
    let identity = if manifest_path.exists() {
        let source = read_manifest_file(&manifest_path)?;
        let manifest = parse_manifest(&source, span)?;
        validate_file_dependency_manifest(&manifest, dependency, span)?;
        FileDependencyIdentity {
            version: manifest
                .version
                .clone()
                .expect("validated file dependency manifest has version"),
            dependencies: manifest.dependencies,
        }
    } else {
        if !matches!(
            parse_version_requirement(&dependency.version, span)?,
            VersionRequirement::Exact(_)
        ) {
            return Err(KuError::package(
                "file_dependency_manifest_required",
                format!(
                    "bare file dependency '{}' must use an exact version; add ku.mod to use a version range",
                    dependency.name
                ),
                span,
            ));
        }
        FileDependencyIdentity {
            version: dependency.version.clone(),
            dependencies: Vec::new(),
        }
    };
    let import_root = source_path.join(DEFAULT_IMPORT_ROOT);
    let metadata = fs::symlink_metadata(&import_root).map_err(|err| {
        KuError::package(
            "missing_package_root",
            format!(
                "file dependency '{}' must contain a '{}' directory: {err}",
                dependency.name, DEFAULT_IMPORT_ROOT
            ),
            span,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KuError::package(
            "invalid_package_root",
            format!(
                "file dependency '{}' import root must be a real '{}' directory",
                dependency.name, DEFAULT_IMPORT_ROOT
            ),
            span,
        ));
    }
    Ok(identity)
}

fn validate_file_dependency_manifest(
    manifest: &KuMod,
    dependency: &PackageDependency,
    span: Span,
) -> KuResult<()> {
    let actual_version = manifest.version.as_deref().ok_or_else(|| {
        KuError::package(
            "missing_package_version",
            format!(
                "file dependency '{}' ku.mod must contain version",
                dependency.name
            ),
            span,
        )
    })?;
    if manifest.name != dependency.name
        || !registry_version_satisfies_all(
            actual_version,
            std::slice::from_ref(&dependency.version),
            span,
        )?
    {
        return Err(KuError::package(
            "package_identity_mismatch",
            format!(
                "file dependency '{}@{}' does not satisfy declared requirement '{}@{}'",
                manifest.name, actual_version, dependency.name, dependency.version
            ),
            span,
        ));
    }
    if manifest.root.as_deref().unwrap_or(DEFAULT_IMPORT_ROOT) != DEFAULT_IMPORT_ROOT {
        return Err(KuError::package(
            "unsupported_package_root",
            format!(
                "file dependency '{}' must use the single package import root '{}'",
                dependency.name, DEFAULT_IMPORT_ROOT
            ),
            span,
        ));
    }
    validate_registry_package_dependencies(&manifest.dependencies)
}

fn resolve_file_dependency_with_mode(
    package: &PackageContext,
    dependency: &PackageDependency,
    source: &str,
    mode: DependencyResolveMode,
    locked: Option<&LockedFileDependency>,
    deadline: Instant,
) -> KuResult<ResolvedFileDependency> {
    ensure_registry_deadline(deadline, Span::default())?;
    if matches!(
        mode,
        DependencyResolveMode::Update | DependencyResolveMode::Refresh
    ) {
        return resolve_file_dependency(package, dependency, source, deadline);
    }
    let locked = locked.ok_or_else(|| {
        KuError::package(
            "lock_out_of_date",
            format!(
                "ku.lock does not contain file dependency '{}'",
                dependency.name
            ),
            Span::default(),
        )
    })?;
    if locked.name != dependency.name
        || locked.requirement != dependency.version
        || locked.source != source
        || dependency
            .checksum
            .as_ref()
            .is_some_and(|checksum| checksum != &locked.checksum)
    {
        return Err(KuError::package(
            "lock_out_of_date",
            format!(
                "ku.lock file dependency '{}' does not match ku.mod",
                dependency.name
            ),
            Span::default(),
        ));
    }
    let expected_cache_key = file_cache_key(&locked.name, &locked.version, &locked.checksum);
    if locked.cache_key != expected_cache_key {
        return Err(KuError::package(
            "invalid_cache_key",
            format!("ku.lock has an invalid cache key for '{}'", dependency.name),
            Span::default(),
        ));
    }
    let target = file_dependency_cache_root(package, &dependency.name, &locked.cache_key);
    if let Some(snapshot) = installed_file_cache_snapshot(
        &target,
        dependency,
        &locked.version,
        &locked.checksum,
        Span::default(),
    )? {
        ensure_registry_deadline(deadline, Span::default())?;
        let identity = inspect_file_dependency_identity(&target, dependency, Span::default())?;
        return Ok(resolved_file_dependency(
            dependency,
            source,
            locked.version.clone(),
            locked.checksum.clone(),
            locked.cache_key.clone(),
            target,
            identity.dependencies,
            snapshot.bytes,
        ));
    }
    if mode == DependencyResolveMode::Offline {
        return Err(KuError::package(
            "offline_cache_miss",
            format!(
                "offline file dependency cache is missing for '{}'",
                dependency.name
            ),
            Span::default(),
        ));
    }
    let source_path = file_url_path(source).ok_or_else(|| {
        KuError::package(
            "unsupported_source",
            format!("dependency '{}' must use file://", dependency.name),
            Span::default(),
        )
    })?;
    let source_path = fs::canonicalize(&source_path).map_err(|err| {
        KuError::message(format!(
            "failed to resolve dependency '{}' source '{}': {err}",
            dependency.name,
            source_path.display()
        ))
    })?;
    validate_file_dependency_source_is_external(package, &source_path)?;
    let identity = inspect_file_dependency_identity(&source_path, dependency, Span::default())?;
    let actual_snapshot = package_source_snapshot(&source_path)?;
    ensure_registry_deadline(deadline, Span::default())?;
    if actual_snapshot.checksum != locked.checksum || identity.version != locked.version {
        return Err(KuError::package(
            "locked_source_changed",
            format!(
                "file dependency '{}' changed after ku.lock was written",
                dependency.name
            ),
            Span::default(),
        ));
    }
    let resolved = install_file_dependency(
        package,
        dependency,
        source,
        &source_path,
        identity,
        actual_snapshot,
        deadline,
    )?;
    if resolved.cache_key != locked.cache_key {
        return Err(KuError::package(
            "locked_source_changed",
            format!(
                "file dependency '{}' cache does not match ku.lock",
                dependency.name
            ),
            Span::default(),
        ));
    }
    Ok(resolved)
}

fn resolve_file_dependency(
    package: &PackageContext,
    dependency: &PackageDependency,
    source: &str,
    deadline: Instant,
) -> KuResult<ResolvedFileDependency> {
    ensure_registry_deadline(deadline, Span::default())?;
    let source_path = file_url_path(source).ok_or_else(|| {
        KuError::package(
            "unsupported_source",
            format!(
                "dependency '{}' uses unsupported source '{}'; use file:// for local development or omit source for the configured registry",
                dependency.name, source
            ),
            Span::default(),
        )
    })?;
    let source_path = fs::canonicalize(&source_path).map_err(|err| {
        KuError::message(format!(
            "failed to resolve dependency '{}' source '{}': {err}",
            dependency.name,
            source_path.display()
        ))
    })?;
    validate_file_dependency_source_is_external(package, &source_path)?;
    let identity = inspect_file_dependency_identity(&source_path, dependency, Span::default())?;
    let actual_snapshot = package_source_snapshot(&source_path)?;
    ensure_registry_deadline(deadline, Span::default())?;
    if let Some(expected) = &dependency.checksum {
        if expected != &actual_snapshot.checksum {
            return Err(KuError::package(
                "checksum_mismatch",
                format!(
                    "dependency '{}' checksum mismatch: expected {}, got {}",
                    dependency.name, expected, actual_snapshot.checksum
                ),
                Span::default(),
            ));
        }
    }
    install_file_dependency(
        package,
        dependency,
        source,
        &source_path,
        identity,
        actual_snapshot,
        deadline,
    )
}

fn install_file_dependency(
    package: &PackageContext,
    dependency: &PackageDependency,
    source: &str,
    source_path: &Path,
    identity: FileDependencyIdentity,
    actual_snapshot: PackageSourceSnapshot,
    deadline: Instant,
) -> KuResult<ResolvedFileDependency> {
    let cache_key = file_cache_key(
        &dependency.name,
        &identity.version,
        &actual_snapshot.checksum,
    );
    let target = file_dependency_cache_root(package, &dependency.name, &cache_key);
    if let Some(snapshot) = installed_file_cache_snapshot(
        &target,
        dependency,
        &identity.version,
        &actual_snapshot.checksum,
        Span::default(),
    )? {
        return Ok(resolved_file_dependency(
            dependency,
            source,
            identity.version,
            actual_snapshot.checksum,
            cache_key,
            target,
            identity.dependencies,
            snapshot.bytes,
        ));
    }
    let parent = target.parent().ok_or_else(|| {
        KuError::package(
            "invalid_cache_path",
            "file dependency cache target must have a parent",
            Span::default(),
        )
    })?;
    fs::create_dir_all(parent).map_err(|err| {
        KuError::message(format!(
            "failed to create dependency cache '{}': {err}",
            parent.display()
        ))
    })?;
    let lock_path =
        package_operation_lock_path(&package.cache_dir, "file", &dependency.name, &cache_key);
    let _lock = acquire_registry_install_lock_until(&lock_path, Span::default(), deadline)?;
    if let Some(snapshot) = installed_file_cache_snapshot(
        &target,
        dependency,
        &identity.version,
        &actual_snapshot.checksum,
        Span::default(),
    )? {
        return Ok(resolved_file_dependency(
            dependency,
            source,
            identity.version,
            actual_snapshot.checksum,
            cache_key,
            target,
            identity.dependencies,
            snapshot.bytes,
        ));
    }
    let copy_id = operation_nonce();
    let tmp = parent.join(format!(".{cache_key}-{copy_id}.file-copy"));
    fs::create_dir(&tmp).map_err(|err| {
        KuError::message(format!(
            "failed to create dependency temp cache '{}': {err}",
            tmp.display()
        ))
    })?;
    let mut temporary = TemporaryDirectory::new(tmp.clone());
    copy_package_source(source_path, &tmp)?;
    ensure_registry_deadline(deadline, Span::default())?;
    let copied_snapshot = package_source_snapshot(&tmp)?;
    if copied_snapshot != actual_snapshot {
        return Err(KuError::package(
            "source_changed",
            format!(
                "dependency '{}' changed while it was being copied; retry the command",
                dependency.name
            ),
            Span::default(),
        ));
    }
    let copied_identity = inspect_file_dependency_identity(&tmp, dependency, Span::default())?;
    if copied_identity.version != identity.version
        || normalized_registry_dependencies(&copied_identity.dependencies)
            != normalized_registry_dependencies(&identity.dependencies)
    {
        return Err(KuError::package(
            "source_changed",
            format!(
                "dependency '{}' identity changed while it was being copied; retry the command",
                dependency.name
            ),
            Span::default(),
        ));
    }
    fs::rename(&tmp, &target).map_err(|err| {
        KuError::message(format!(
            "failed to install dependency cache '{}': {err}",
            target.display()
        ))
    })?;
    temporary.keep();
    ensure_registry_deadline(deadline, Span::default())?;
    Ok(resolved_file_dependency(
        dependency,
        source,
        identity.version,
        actual_snapshot.checksum,
        cache_key,
        target,
        identity.dependencies,
        actual_snapshot.bytes,
    ))
}

#[allow(clippy::too_many_arguments)]
fn resolved_file_dependency(
    dependency: &PackageDependency,
    source: &str,
    version: String,
    checksum: String,
    cache_key: String,
    package_root: PathBuf,
    dependencies: Vec<PackageDependency>,
    snapshot_bytes: u64,
) -> ResolvedFileDependency {
    ResolvedFileDependency {
        name: dependency.name.clone(),
        requirement: dependency.version.clone(),
        version,
        source: source.to_string(),
        checksum,
        cache_key,
        package_root,
        dependencies,
        snapshot_bytes,
    }
}

fn installed_file_cache_snapshot(
    target: &Path,
    dependency: &PackageDependency,
    version: &str,
    checksum: &str,
    span: Span,
) -> KuResult<Option<PackageSourceSnapshot>> {
    installed_file_cache_snapshot_with_budget(target, dependency, version, checksum, span, None)
}

fn installed_file_cache_snapshot_with_budget(
    target: &Path,
    dependency: &PackageDependency,
    version: &str,
    checksum: &str,
    span: Span,
    gc_budget: Option<&mut CacheGcScanBudget>,
) -> KuResult<Option<PackageSourceSnapshot>> {
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(KuError::package(
                "file_cache_tampered",
                format!("failed to inspect file dependency cache: {err}"),
                span,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KuError::package(
            "file_cache_tampered",
            format!(
                "file dependency '{}' cache is not a real directory",
                dependency.name
            ),
            span,
        ));
    }
    let snapshot = package_source_snapshot_with_budget(target, gc_budget)?;
    if snapshot.checksum != checksum {
        return Err(KuError::package(
            "file_cache_tampered",
            format!(
                "file dependency '{}' immutable cache was modified",
                dependency.name
            ),
            span,
        ));
    }
    let identity = inspect_file_dependency_identity(target, dependency, span)?;
    if identity.version != version {
        return Err(KuError::package(
            "file_cache_tampered",
            format!(
                "file dependency '{}' cache version was modified",
                dependency.name
            ),
            span,
        ));
    }
    Ok(Some(snapshot))
}

fn fetch_trusted_registry_index_until(
    registry: &PackageRegistry,
    name: &str,
    span: Span,
    deadline: Instant,
    http_client: &RegistryHttpClient,
) -> KuResult<RegistryIndex> {
    Ok(
        fetch_trusted_registry_index_with_usage_until(registry, name, span, deadline, http_client)?
            .index,
    )
}

fn fetch_trusted_registry_index_with_usage_until(
    registry: &PackageRegistry,
    name: &str,
    span: Span,
    deadline: Instant,
    http_client: &RegistryHttpClient,
) -> KuResult<CachedRegistryIndex> {
    validate_package_name(name, span)?;
    let index_url = registry_index_url(&registry.url, name, span)?;
    let signature_url = format!("{index_url}.sig");
    validate_registry_url(&signature_url, span)?;
    let public_key = parse_ed25519_public_key(&registry.public_key, span)?;
    const MAX_INDEX_PAIR_ATTEMPTS: usize = 3;
    for pair_attempt in 1..=MAX_INDEX_PAIR_ATTEMPTS {
        ensure_registry_deadline(deadline, span)?;
        let signature_policy =
            registry_fetch_policy_for_deadline(deadline, MAX_REGISTRY_SIGNATURE_BYTES, span)?;
        let signature_bytes = fetch_https_bytes_with_client(
            http_client,
            &signature_url,
            signature_policy,
            deadline,
            span,
        )?;
        let signature_text = std::str::from_utf8(&signature_bytes).map_err(|_| {
            KuError::package(
                "invalid_registry_signature",
                "registry index signature must be valid UTF-8",
                span,
            )
        })?;
        let signature = parse_ed25519_signature(signature_text.trim(), span)?;
        let verifier = Ed25519RegistryIndexVerifier::new(public_key, signature);
        let index_policy =
            registry_fetch_policy_for_deadline(deadline, MAX_REGISTRY_INDEX_BYTES, span)?;
        let index_bytes =
            fetch_https_bytes_with_client(http_client, &index_url, index_policy, deadline, span)?;
        let verified = verifier.verify(&index_url, &index_bytes, span);
        ensure_registry_deadline(deadline, span)?;
        match verified {
            Ok(()) => {
                let parsed = (|| {
                    let source = std::str::from_utf8(&index_bytes).map_err(|_| {
                        KuError::package(
                            "invalid_registry_index",
                            "registry index must be valid UTF-8",
                            span,
                        )
                    })?;
                    let index = parse_registry_index(source, &index_url, span)?;
                    let source_bytes = u64::try_from(index_bytes.len()).map_err(|_| {
                        registry_index_graph_limit_error(
                            "signed registry index byte count cannot be represented",
                        )
                    })?;
                    let usage = RegistryIndexResourceUsage::from_index(&index, source_bytes)?;
                    Ok(CachedRegistryIndex { index, usage })
                })();
                ensure_registry_deadline(deadline, span)?;
                return parsed;
            }
            Err(err)
                if err.code.as_deref() == Some("registry_signature_mismatch")
                    && pair_attempt < MAX_INDEX_PAIR_ATTEMPTS =>
            {
                ensure_registry_deadline(deadline, span)?;
                let delay = Duration::from_millis(10 * pair_attempt as u64);
                if Instant::now()
                    .checked_add(delay)
                    .is_none_or(|next| next >= deadline)
                {
                    return Err(registry_resolve_timeout_error(span));
                }
                thread::sleep(delay);
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("bounded index pair loop always returns")
}

fn registry_fetch_policy_for_deadline(
    deadline: Instant,
    max_download_bytes: u64,
    span: Span,
) -> KuResult<RegistryFetchPolicy> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| registry_resolve_timeout_error(span))?;
    let remaining_ms = u64::try_from(remaining.as_millis())
        .unwrap_or(u64::MAX)
        .saturating_sub(100);
    if remaining_ms < 2 {
        return Err(KuError::package(
            "registry_resolve_timeout",
            format!("registry operation exceeded its {MAX_REGISTRY_RESOLVE_SECS} second deadline"),
            span,
        ));
    }
    let defaults = RegistryFetchPolicy::default();
    let mut attempts = defaults.max_attempts;
    while attempts > 1 && remaining_ms / u64::from(attempts) < 2 {
        attempts -= 1;
    }
    let per_attempt = remaining_ms / u64::from(attempts);
    let connect_timeout_ms = defaults.connect_timeout_ms.min((per_attempt / 3).max(1));
    let read_timeout_ms = defaults
        .read_timeout_ms
        .min(per_attempt.saturating_sub(connect_timeout_ms).max(1));
    Ok(RegistryFetchPolicy {
        max_attempts: attempts,
        connect_timeout_ms,
        read_timeout_ms,
        max_download_bytes,
    })
}

fn ensure_registry_deadline(deadline: Instant, span: Span) -> KuResult<()> {
    if Instant::now() >= deadline {
        Err(registry_resolve_timeout_error(span))
    } else {
        Ok(())
    }
}

fn registry_resolve_timeout_error(span: Span) -> KuError {
    KuError::package(
        "registry_resolve_timeout",
        format!("registry operation exceeded its {MAX_REGISTRY_RESOLVE_SECS} second deadline"),
        span,
    )
}

fn registry_index_url(base_url: &str, name: &str, span: Span) -> KuResult<String> {
    validate_registry_base_url(base_url, span)?;
    validate_package_name(name, span)?;
    resolve_registry_url(base_url, &format!("packages/{name}/index.toml"), span)
}

fn read_locked_registry_dependencies(
    package: &PackageContext,
) -> KuResult<HashMap<String, LockedRegistryDependency>> {
    let mut output = HashMap::new();
    for fields in read_locked_package_dependency_fields(package)? {
        finish_locked_registry_dependency(fields, &mut output, Span::default())?;
    }
    Ok(output)
}

fn read_locked_file_dependencies(
    package: &PackageContext,
) -> KuResult<HashMap<String, LockedFileDependency>> {
    let mut output = HashMap::new();
    for fields in read_locked_package_dependency_fields(package)? {
        if fields.get("source").map(String::as_str) == Some("registry") {
            continue;
        }
        reject_unknown_fields(
            &fields,
            &[
                "name",
                "requirement",
                "version",
                "cache",
                "source",
                "checksum",
                "cache_key",
            ],
            "ku.lock file dependency",
            Span::default(),
        )?;
        let name = required_field(&fields, "name", "ku.lock file dependency", Span::default())?;
        let version = required_field(
            &fields,
            "version",
            "ku.lock file dependency",
            Span::default(),
        )?;
        let requirement = fields
            .get("requirement")
            .cloned()
            .unwrap_or_else(|| version.clone());
        let source = required_field(
            &fields,
            "source",
            "ku.lock file dependency",
            Span::default(),
        )?;
        let checksum = required_field(
            &fields,
            "checksum",
            "ku.lock file dependency",
            Span::default(),
        )?;
        validate_package_name(&name, Span::default())?;
        validate_version_requirement(&requirement, Span::default())?;
        validate_version(&version, Span::default())?;
        if file_url_path(&source).is_none() {
            return Err(KuError::package(
                "invalid_package_lock",
                format!("ku.lock file dependency '{name}' has an invalid source"),
                Span::default(),
            ));
        }
        validate_checksum(&checksum, Span::default())?;
        let expected_cache_key = file_cache_key(&name, &version, &checksum);
        let cache_key = fields
            .get("cache_key")
            .cloned()
            .unwrap_or_else(|| expected_cache_key.clone());
        if cache_key != expected_cache_key {
            return Err(KuError::package(
                "invalid_cache_key",
                format!("ku.lock file dependency has invalid cache_key for '{name}'"),
                Span::default(),
            ));
        }
        if output
            .insert(
                name.clone(),
                LockedFileDependency {
                    name: name.clone(),
                    requirement,
                    version,
                    source,
                    checksum,
                    cache_key,
                },
            )
            .is_some()
        {
            return Err(KuError::package(
                "duplicate_package_lock_dependency",
                format!("ku.lock contains duplicate file dependency '{name}'"),
                Span::default(),
            ));
        }
    }
    Ok(output)
}

fn read_locked_package_dependency_fields(
    package: &PackageContext,
) -> KuResult<Vec<HashMap<String, String>>> {
    let _lock =
        acquire_registry_install_lock(&package_lock_io_path(&package.lock_path), Span::default())?;
    let metadata = match fs::symlink_metadata(&package.lock_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to inspect package lock '{}': {err}",
                package.lock_path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_PACKAGE_LOCK_BYTES
    {
        return Err(KuError::package(
            "package_lock_limit",
            format!("ku.lock must be a regular file no larger than {MAX_PACKAGE_LOCK_BYTES} bytes"),
            Span::default(),
        ));
    }
    let file = fs::File::open(&package.lock_path).map_err(|err| {
        KuError::message(format!(
            "failed to open package lock '{}': {err}",
            package.lock_path.display()
        ))
    })?;
    let mut source = String::new();
    file.take(MAX_PACKAGE_LOCK_BYTES + 1)
        .read_to_string(&mut source)
        .map_err(|err| {
            KuError::message(format!(
                "failed to read package lock '{}': {err}",
                package.lock_path.display()
            ))
        })?;
    if source.len() as u64 > MAX_PACKAGE_LOCK_BYTES {
        return Err(KuError::package(
            "package_lock_limit",
            format!("ku.lock exceeds {MAX_PACKAGE_LOCK_BYTES} bytes"),
            Span::default(),
        ));
    }
    let mut output = Vec::new();
    let mut current = None::<HashMap<String, String>>;
    for (index, raw_line) in source.lines().enumerate() {
        if raw_line.len() > MAX_REGISTRY_LINE_BYTES {
            return Err(KuError::package(
                "package_lock_limit",
                format!(
                    "ku.lock line {} exceeds {MAX_REGISTRY_LINE_BYTES} bytes",
                    index + 1
                ),
                Span::default(),
            ));
        }
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("[[") && line.ends_with("]]") {
            if let Some(fields) = current.take() {
                output.push(fields);
            }
            if line == "[[package_dependency]]" {
                if output.len() >= MAX_REGISTRY_DEPENDENCIES {
                    return Err(KuError::package(
                        "dependency_limit",
                        format!(
                            "ku.lock contains more than {MAX_REGISTRY_DEPENDENCIES} package dependencies"
                        ),
                        Span::default(),
                    ));
                }
                current = Some(HashMap::new());
            }
            continue;
        }
        let Some(fields) = current.as_mut() else {
            continue;
        };
        let Some((key, raw_value)) = line.split_once('=') else {
            return Err(KuError::package(
                "invalid_package_lock",
                format!("invalid ku.lock line {}: expected key = value", index + 1),
                Span::default(),
            ));
        };
        let key = key.trim().to_string();
        let value = parse_string_value(raw_value.trim(), index + 1, Span::default())?;
        if fields.insert(key.clone(), value).is_some() {
            return Err(KuError::package(
                "duplicate_package_lock_field",
                format!("duplicate ku.lock package dependency field '{key}'"),
                Span::default(),
            ));
        }
    }
    if let Some(fields) = current {
        output.push(fields);
    }
    Ok(output)
}

fn finish_locked_registry_dependency(
    fields: HashMap<String, String>,
    output: &mut HashMap<String, LockedRegistryDependency>,
    span: Span,
) -> KuResult<()> {
    if fields.get("source").map(String::as_str) != Some("registry") {
        return Ok(());
    }
    reject_unknown_fields(
        &fields,
        &[
            "name",
            "requirement",
            "version",
            "cache",
            "source",
            "registry",
            "url",
            "checksum",
            "cache_key",
        ],
        "ku.lock registry dependency",
        span,
    )?;
    let name = required_field(&fields, "name", "ku.lock registry dependency", span)?;
    let version = required_field(&fields, "version", "ku.lock registry dependency", span)?;
    let registry = required_field(&fields, "registry", "ku.lock registry dependency", span)?;
    let url = required_field(&fields, "url", "ku.lock registry dependency", span)?;
    let checksum = required_field(&fields, "checksum", "ku.lock registry dependency", span)?;
    let cache_key = required_field(&fields, "cache_key", "ku.lock registry dependency", span)?;
    validate_package_name(&name, span)?;
    validate_version(&version, span)?;
    validate_registry_base_url(&registry, span)?;
    validate_registry_url(&url, span)?;
    validate_registry_archive_url(&url, span)?;
    validate_sha256_checksum(&checksum, span)?;
    let expected_cache_key = registry_cache_key(&RegistryManifest {
        name: name.clone(),
        version: version.clone(),
        source: url.clone(),
        checksum: checksum.clone(),
        dependencies: Vec::new(),
    });
    if cache_key != expected_cache_key {
        return Err(KuError::package(
            "invalid_cache_key",
            format!("ku.lock registry dependency has invalid cache_key for '{name}'"),
            span,
        ));
    }
    if output
        .insert(
            name.clone(),
            LockedRegistryDependency {
                name: name.clone(),
                version,
                registry,
                url,
                checksum,
                cache_key,
            },
        )
        .is_some()
    {
        return Err(KuError::package(
            "duplicate_package_lock_dependency",
            format!("ku.lock contains duplicate registry dependency '{name}'"),
            span,
        ));
    }
    Ok(())
}

pub(crate) fn package_import_scopes(
    package: &PackageContext,
    span: Span,
) -> KuResult<Vec<PackageImportScope>> {
    let mut scopes = Vec::with_capacity(
        1 + package.resolved_file_dependencies.len() + package.resolved_registry_dependencies.len(),
    );
    scopes.push(package_import_scope(
        &package.package_dir,
        &package.manifest,
        true,
        span,
    )?);

    for dependency in &package.resolved_file_dependencies {
        let manifest = KuMod {
            name: dependency.name.clone(),
            version: Some(dependency.version.clone()),
            root: None,
            main: None,
            out: None,
            cache: None,
            template: None,
            package_type: None,
            registry: None,
            dependencies: dependency.dependencies.clone(),
        };
        scopes.push(package_import_scope(
            &dependency.package_root,
            &manifest,
            false,
            span,
        )?);
    }

    for dependency in &package.resolved_registry_dependencies {
        let manifest = validate_package_root_identity(
            &dependency.package_root,
            &dependency.name,
            &dependency.version,
            span,
        )?;
        scopes.push(package_import_scope(
            &dependency.package_root,
            &manifest,
            false,
            span,
        )?);
    }

    let mut owners_by_root = HashMap::<PathBuf, String>::new();
    let mut owners_by_name = HashMap::<String, PathBuf>::new();
    for scope in &scopes {
        if let Some(existing) =
            owners_by_name.insert(scope.package_name.clone(), scope.import_root.clone())
        {
            return Err(KuError::package(
                "duplicate_package_name",
                format!(
                    "package name '{}' resolves to both '{}' and '{}'",
                    scope.package_name,
                    existing.display(),
                    scope.import_root.display()
                ),
                span,
            ));
        }
        if let Some(existing) =
            owners_by_root.insert(scope.import_root.clone(), scope.package_name.clone())
        {
            return Err(KuError::package(
                "duplicate_package_root",
                format!(
                    "packages '{}' and '{}' resolve to the same import root",
                    existing, scope.package_name
                ),
                span,
            ));
        }
    }
    scopes.sort_by(|left, right| {
        right
            .import_root
            .components()
            .count()
            .cmp(&left.import_root.components().count())
            .then_with(|| left.package_name.cmp(&right.package_name))
    });
    Ok(scopes)
}

fn package_import_scope(
    package_root: &Path,
    manifest: &KuMod,
    is_consumer: bool,
    span: Span,
) -> KuResult<PackageImportScope> {
    let package_root = fs::canonicalize(package_root).map_err(|err| {
        KuError::package(
            "missing_package_root",
            format!(
                "failed to resolve package '{}' root '{}': {err}",
                manifest.name,
                package_root.display()
            ),
            span,
        )
    })?;
    let import_root_path =
        package_root.join(manifest.root.as_deref().unwrap_or(DEFAULT_IMPORT_ROOT));
    let metadata = fs::symlink_metadata(&import_root_path).map_err(|err| {
        KuError::package(
            "missing_package_root",
            format!(
                "package '{}' import root '{}' is unavailable: {err}",
                manifest.name,
                import_root_path.display()
            ),
            span,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KuError::package(
            "invalid_package_root",
            format!(
                "package '{}' import root must be a real directory",
                manifest.name
            ),
            span,
        ));
    }
    let import_root = fs::canonicalize(&import_root_path).map_err(|err| {
        KuError::package(
            "missing_package_root",
            format!(
                "failed to resolve package '{}' import root '{}': {err}",
                manifest.name,
                import_root_path.display()
            ),
            span,
        )
    })?;
    if !import_root.starts_with(&package_root) {
        return Err(KuError::package(
            "unsafe_package_root",
            format!(
                "package '{}' import root escapes its package",
                manifest.name
            ),
            span,
        ));
    }
    Ok(PackageImportScope {
        package_name: manifest.name.clone(),
        import_root,
        dependencies: manifest.dependencies.clone(),
        is_consumer,
    })
}

pub(crate) fn package_import_scope_for_file<'a>(
    scopes: &'a [PackageImportScope],
    current_file: &Path,
    span: Span,
) -> KuResult<&'a PackageImportScope> {
    let canonical = fs::canonicalize(current_file).map_err(|err| {
        KuError::runtime(
            format!(
                "failed to resolve current import module '{}': {err}",
                current_file.display()
            ),
            span,
        )
    })?;
    scopes
        .iter()
        .find(|scope| canonical.starts_with(&scope.import_root))
        .ok_or_else(|| {
            KuError::runtime(
                format!(
                    "module '{}' is outside every package import root",
                    current_file.display()
                ),
                span,
            )
        })
}

pub(crate) fn canonical_import_in_scope(
    path: &Path,
    scope: &PackageImportScope,
    span: Span,
) -> KuResult<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|err| {
        KuError::runtime(
            format!("failed to resolve import '{}': {err}", path.display()),
            span,
        )
    })?;
    if canonical.starts_with(&scope.import_root) {
        Ok(canonical)
    } else {
        Err(KuError::runtime(
            format!(
                "import '{}' is outside package '{}' import root '{}'",
                path.display(),
                scope.package_name,
                scope.import_root.display()
            ),
            span,
        ))
    }
}

pub(crate) fn resolve_dependency_import(
    scopes: &[PackageImportScope],
    current_scope: &PackageImportScope,
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
    let declared_dependency = current_scope
        .dependencies
        .iter()
        .find(|dep| dep.name == name);
    let Some(declared_dependency) = declared_dependency else {
        return Err(KuError::package(
            "unknown_dependency",
            format!(
                "package '{}' does not declare dependency '{name}'",
                current_scope.package_name
            ),
            span,
        ));
    };
    reject_unsafe_dependency_import(relative, span)?;
    if declared_dependency.source.is_some() && !current_scope.is_consumer {
        return Err(KuError::package(
            "unsupported_source",
            format!(
                "dependency package '{}' cannot expose a transitive file:// dependency; publish registry dependencies instead",
                current_scope.package_name
            ),
            span,
        ));
    }
    let target_scope = scopes
        .iter()
        .find(|scope| !scope.is_consumer && scope.package_name == name)
        .ok_or_else(|| {
            KuError::package(
                "unresolved_dependency",
                format!(
                    "dependency '{}' declared by package '{}' was not resolved",
                    name, current_scope.package_name
                ),
                span,
            )
        })?;
    let mut path = target_scope.import_root.join(relative);
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
    Ok(Some(canonical_import_in_scope(&path, target_scope, span)?))
}

pub fn package_source_checksum(path: &Path) -> KuResult<String> {
    Ok(package_source_snapshot(path)?.checksum)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageSourceSnapshot {
    checksum: String,
    bytes: u64,
}

fn package_source_snapshot(path: &Path) -> KuResult<PackageSourceSnapshot> {
    package_source_snapshot_with_budget(path, None)
}

fn package_source_snapshot_with_budget(
    path: &Path,
    gc_budget: Option<&mut CacheGcScanBudget>,
) -> KuResult<PackageSourceSnapshot> {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut files = 0;
    let mut total_bytes = 0;
    let mut gc_budget = gc_budget;
    collect_source_hash(
        path,
        path,
        &mut hash,
        &mut files,
        &mut total_bytes,
        0,
        &mut gc_budget,
    )?;
    Ok(PackageSourceSnapshot {
        checksum: format!("ku-fnv64-{hash:016x}"),
        bytes: total_bytes,
    })
}

#[derive(Debug)]
struct ArchiveSourceEntry {
    relative: String,
    is_dir: bool,
    size: u64,
    identity: Option<ArchiveFileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArchiveFileIdentity {
    volume: u64,
    file: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}

#[derive(Debug)]
struct CollectedPackageArchive {
    source_root: ArchiveSourceRoot,
    entries: Vec<ArchiveSourceEntry>,
}

#[derive(Debug)]
struct ArchiveSourceRoot {
    canonical: PathBuf,
    _handle: fs::File,
    #[cfg(windows)]
    final_path: String,
}

pub fn pack_package(package: &PackageContext) -> KuResult<PackageArtifact> {
    pack_package_with_policy(package, PackageArchivePolicy::default())
}

fn pack_package_with_policy(
    package: &PackageContext,
    policy: PackageArchivePolicy,
) -> KuResult<PackageArtifact> {
    let span = Span::default();
    validate_package_archive_policy(policy, span)?;
    let version = package.manifest.version.as_deref().ok_or_else(|| {
        KuError::package(
            "missing_package_version",
            "ku package pack requires version in ku.mod",
            span,
        )
    })?;
    if package
        .manifest
        .root
        .as_deref()
        .unwrap_or(DEFAULT_IMPORT_ROOT)
        != DEFAULT_IMPORT_ROOT
    {
        return Err(KuError::package(
            "unsupported_publish_root",
            "published packages use the single standard import root 'src'",
            span,
        ));
    }
    validate_registry_package_dependencies(&package.manifest.dependencies)?;
    validate_package_root_identity(&package.package_dir, &package.manifest.name, version, span)?;

    let distribution_dir = package.package_dir.join(PACKAGE_DISTRIBUTION_DIR);
    fs::create_dir_all(&distribution_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create package artifact directory '{}': {err}",
            distribution_dir.display()
        ))
    })?;
    let canonical_package = fs::canonicalize(&package.package_dir).map_err(|err| {
        KuError::message(format!(
            "failed to resolve package directory '{}': {err}",
            package.package_dir.display()
        ))
    })?;
    let canonical_distribution = fs::canonicalize(&distribution_dir).map_err(|err| {
        KuError::message(format!(
            "failed to resolve package artifact directory '{}': {err}",
            distribution_dir.display()
        ))
    })?;
    if !canonical_distribution.starts_with(&canonical_package) {
        return Err(KuError::package(
            "unsafe_artifact_path",
            "package artifact directory must stay inside the package",
            span,
        ));
    }
    cleanup_stale_pack_staging(
        &distribution_dir,
        Duration::from_secs(REGISTRY_STAGING_STALE_SECS),
        MAX_PACK_STAGING_SCAN_ENTRIES,
        policy,
    )?;
    let collected = collect_package_archive_entries(package, policy, span)?;

    let archive_id = operation_nonce();
    let temporary_path = distribution_dir.join(format!(
        ".{}-{}-{archive_id}.tar.zst.tmp",
        package.manifest.name, version
    ));
    let mut temporary = TemporaryFile::new(temporary_path.clone());
    write_deterministic_package_archive(
        &temporary_path,
        &package.manifest.name,
        version,
        &collected,
        policy.max_compressed_bytes,
        span,
    )?;
    let metadata = fs::metadata(&temporary_path).map_err(|err| {
        KuError::message(format!(
            "failed to inspect package artifact '{}': {err}",
            temporary_path.display()
        ))
    })?;
    if metadata.len() > policy.max_compressed_bytes {
        return Err(KuError::package(
            "package_archive_limit",
            format!(
                "package artifact exceeds compressed limit of {} bytes",
                policy.max_compressed_bytes
            ),
            span,
        ));
    }
    let digest = sha256_file(&temporary_path, policy.max_compressed_bytes)?;
    let checksum = format!("sha256-{digest}");
    let verification_path = distribution_dir.join(format!(
        ".{}-{}-{archive_id}.verify",
        package.manifest.name, version
    ));
    let _verification = TemporaryDirectory::new(verification_path.clone());
    unpack_package_archive_for_identity(
        &temporary_path,
        &verification_path,
        policy,
        &package.manifest.name,
        version,
        None,
        span,
    )?;
    validate_package_root_identity(&verification_path, &package.manifest.name, version, span)?;
    let archived_tree = package_archive_tree_sha256(&temporary_path, policy, span)?;
    let unpacked_tree = package_tree_sha256(&verification_path, policy, span)?;
    if archived_tree != unpacked_tree {
        return Err(KuError::package(
            "package_archive_integrity",
            "packed artifact did not round-trip to the same source tree",
            span,
        ));
    }
    let artifact_path = distribution_dir.join(format!(
        "{}-{}-sha256-{}.tar.zst",
        package.manifest.name, version, digest
    ));
    if artifact_path.exists() {
        if !sha256_file_matches(&artifact_path, &checksum, policy.max_compressed_bytes)? {
            return Err(KuError::package(
                "package_artifact_conflict",
                format!(
                    "package artifact path exists with different content '{}'",
                    artifact_path.display()
                ),
                span,
            ));
        }
    } else if let Err(err) = fs::rename(&temporary_path, &artifact_path) {
        if !artifact_path.exists()
            || !sha256_file_matches(&artifact_path, &checksum, policy.max_compressed_bytes)?
        {
            return Err(KuError::message(format!(
                "failed to install package artifact '{}': {err}",
                artifact_path.display()
            )));
        }
    }
    temporary.keep();
    if temporary_path.exists() {
        fs::remove_file(&temporary_path).map_err(|err| {
            KuError::message(format!(
                "failed to remove duplicate package artifact staging '{}': {err}",
                temporary_path.display()
            ))
        })?;
    }
    Ok(PackageArtifact {
        path: artifact_path,
        name: package.manifest.name.clone(),
        version: version.to_string(),
        checksum,
        size: metadata.len(),
    })
}

pub fn plan_package_publish(
    package: &PackageContext,
    artifact: &PackageArtifact,
) -> KuResult<PackagePublishPlan> {
    let span = Span::default();
    let registry = package.manifest.registry.as_ref().ok_or_else(|| {
        KuError::package(
            "registry_trust_unconfigured",
            "ku package publish requires registry.url and registry.public_key in ku.mod",
            span,
        )
    })?;
    if artifact.name != package.manifest.name
        || package.manifest.version.as_deref() != Some(artifact.version.as_str())
    {
        return Err(KuError::package(
            "package_identity_mismatch",
            "package artifact identity does not match ku.mod",
            span,
        ));
    }
    validate_sha256_checksum(&artifact.checksum, span)?;
    if artifact.size == 0 || artifact.size > MAX_REGISTRY_DOWNLOAD_BYTES {
        return Err(KuError::package(
            "package_archive_limit",
            format!(
                "package publish content length must be between 1 and {MAX_REGISTRY_DOWNLOAD_BYTES} bytes"
            ),
            span,
        ));
    }
    let url = resolve_registry_url(
        &registry.url,
        &format!("packages/{}/{}", artifact.name, artifact.version),
        span,
    )?;
    Ok(PackagePublishPlan {
        url,
        artifact_path: artifact.path.clone(),
        checksum: artifact.checksum.clone(),
        content_length: artifact.size,
        idempotency_key: format!(
            "{}-{}-{}",
            artifact.name, artifact.version, artifact.checksum
        ),
    })
}

pub fn publish_package(package: &PackageContext, token: &str) -> KuResult<PackagePublishReceipt> {
    let deadline = Instant::now() + Duration::from_secs(MAX_REGISTRY_RESOLVE_SECS);
    validate_publish_token(token, Span::default())?;
    let artifact = pack_package(package)?;
    let plan = plan_package_publish(package, &artifact)?;
    let file = fs::File::open(&plan.artifact_path).map_err(|err| {
        KuError::message(format!(
            "failed to open package artifact '{}': {err}",
            plan.artifact_path.display()
        ))
    })?;
    let policy = registry_fetch_policy_for_deadline(
        deadline,
        RegistryFetchPolicy::default().max_download_bytes,
        Span::default(),
    )?;
    let http_client = RegistryHttpClient::new(policy, &plan.url);
    let authorization = format!("Bearer {token}");
    let response = http_client
        .agent()
        .put(&plan.url)
        .timeout(Duration::from_millis(policy.read_timeout_ms))
        .set("Authorization", &authorization)
        .set("Content-Type", PACKAGE_CONTENT_TYPE)
        .set("Content-Length", &plan.content_length.to_string())
        .set("X-Ku-Checksum", &plan.checksum)
        .set("Idempotency-Key", &plan.idempotency_key)
        .send(file);
    let (status, success_response) = match response {
        Ok(response) => (response.status(), Some(response)),
        Err(ureq::Error::Status(status, _)) => (status, None),
        Err(ureq::Error::Transport(_transport)) => {
            #[cfg(test)]
            eprintln!(
                "registry publish transport diagnostic: {}",
                test_registry_transport_diagnostic(&_transport)
            );
            return Err(KuError::package(
                "registry_publish_failed",
                "registry publish transport failed; the server may have received the idempotent upload",
                Span::default(),
            ));
        }
    };
    validate_publish_http_status(status, &artifact)?;
    if let Some(response) = success_response {
        drain_publish_response(response)?;
    }

    let registry = package
        .manifest
        .registry
        .as_ref()
        .expect("publish plan requires registry configuration");
    let index = fetch_trusted_registry_index_until(
        registry,
        &artifact.name,
        Span::default(),
        deadline,
        &http_client,
    )?;
    verify_published_index(&index, &artifact, &package.manifest.dependencies)?;
    Ok(PackagePublishReceipt {
        name: artifact.name,
        version: artifact.version,
        checksum: artifact.checksum,
        registry: registry.url.clone(),
    })
}

pub fn yank_package(package: &PackageContext, token: &str) -> KuResult<PackageYankReceipt> {
    let deadline = Instant::now() + Duration::from_secs(MAX_REGISTRY_RESOLVE_SECS);
    let span = Span::default();
    validate_publish_token(token, span)?;
    let registry = package.manifest.registry.as_ref().ok_or_else(|| {
        KuError::package(
            "registry_trust_unconfigured",
            "ku package yank requires registry.url and registry.public_key in ku.mod",
            span,
        )
    })?;
    let version = package.manifest.version.as_deref().ok_or_else(|| {
        KuError::package(
            "missing_package_version",
            "ku package yank requires version in ku.mod",
            span,
        )
    })?;
    validate_package_name(&package.manifest.name, span)?;
    validate_version(version, span)?;
    let url = resolve_registry_url(
        &registry.url,
        &format!("packages/{}/{version}/yank", package.manifest.name),
        span,
    )?;
    let policy =
        registry_fetch_policy_for_deadline(deadline, MAX_REGISTRY_PUBLISH_RESPONSE_BYTES, span)?;
    let http_client = RegistryHttpClient::new(policy, &url);
    let authorization = format!("Bearer {token}");
    let response = http_client
        .agent()
        .put(&url)
        .timeout(Duration::from_millis(policy.read_timeout_ms))
        .set("Authorization", &authorization)
        .set("Content-Length", "0")
        .set(
            "Idempotency-Key",
            &format!("yank-{}-{version}", package.manifest.name),
        )
        .call();
    let response = match response {
        Ok(response) if response.status() == 200 => response,
        Ok(response) => {
            return Err(yank_http_status_error(
                response.status(),
                &package.manifest.name,
                version,
            ));
        }
        Err(ureq::Error::Status(status, _)) => {
            return Err(yank_http_status_error(
                status,
                &package.manifest.name,
                version,
            ));
        }
        Err(ureq::Error::Transport(_)) => {
            return Err(KuError::package(
                "registry_yank_failed",
                "registry yank transport failed; the server may have applied the idempotent yank",
                span,
            ));
        }
    };
    drain_yank_response(response)?;
    let index = fetch_trusted_registry_index_until(
        registry,
        &package.manifest.name,
        span,
        deadline,
        &http_client,
    )?;
    if index.name != package.manifest.name
        || index.versions.iter().any(|entry| entry.version == version)
    {
        return Err(KuError::package(
            "yank_verification_failed",
            format!(
                "registry returned success before the signed index withdrew '{}@{version}'",
                package.manifest.name
            ),
            span,
        ));
    }
    Ok(PackageYankReceipt {
        name: package.manifest.name.clone(),
        version: version.to_string(),
        registry: registry.url.clone(),
    })
}

fn yank_http_status_error(status: u16, name: &str, version: &str) -> KuError {
    let (code, message) = match status {
        401 | 403 => (
            "registry_yank_unauthorized",
            format!("registry rejected yank credentials with HTTP status {status}"),
        ),
        404 => (
            "registry_version_not_found",
            format!("registry does not contain '{name}@{version}'"),
        ),
        429 => (
            "registry_yank_limited",
            "registry has too many active package mutations; retry the same idempotent yank"
                .to_string(),
        ),
        _ => (
            "registry_yank_status",
            format!("registry returned yank HTTP status {status}"),
        ),
    };
    KuError::package(code, message, Span::default())
}

fn drain_publish_response(response: ureq::Response) -> KuResult<()> {
    match read_limited(
        response.into_reader(),
        io::sink(),
        MAX_REGISTRY_PUBLISH_RESPONSE_BYTES,
    ) {
        Ok(_) => Ok(()),
        Err(err) if err.code.as_deref() == Some("download_limit") => Err(KuError::package(
            "registry_publish_response_limit",
            format!(
                "registry publish response exceeds {MAX_REGISTRY_PUBLISH_RESPONSE_BYTES} bytes; the idempotent upload may already have succeeded"
            ),
            Span::default(),
        )),
        Err(_) => Err(KuError::package(
            "registry_publish_failed",
            "failed to finish reading the registry publish response; the idempotent upload may already have succeeded",
            Span::default(),
        )),
    }
}

fn drain_yank_response(response: ureq::Response) -> KuResult<()> {
    match read_limited(
        response.into_reader(),
        io::sink(),
        MAX_REGISTRY_PUBLISH_RESPONSE_BYTES,
    ) {
        Ok(_) => Ok(()),
        Err(err) if err.code.as_deref() == Some("download_limit") => Err(KuError::package(
            "registry_yank_response_limit",
            format!(
                "registry yank response exceeds {MAX_REGISTRY_PUBLISH_RESPONSE_BYTES} bytes; the idempotent yank may already have succeeded"
            ),
            Span::default(),
        )),
        Err(_) => Err(KuError::package(
            "registry_yank_failed",
            "failed to finish reading the registry yank response; the idempotent yank may already have succeeded",
            Span::default(),
        )),
    }
}

fn verify_published_index(
    index: &RegistryIndex,
    artifact: &PackageArtifact,
    dependencies: &[PackageDependency],
) -> KuResult<()> {
    if index.name != artifact.name {
        return Err(KuError::package(
            "publish_verification_failed",
            format!(
                "signed registry index name '{}' does not match published package '{}'",
                index.name, artifact.name
            ),
            Span::default(),
        ));
    }
    let committed = index
        .versions
        .iter()
        .find(|entry| entry.version == artifact.version)
        .ok_or_else(|| {
            KuError::package(
                "publish_verification_failed",
                "registry returned success before the signed index exposed the package version",
                Span::default(),
            )
        })?;
    if committed.checksum != artifact.checksum {
        return Err(KuError::package(
            "publish_verification_failed",
            format!(
                "signed registry index checksum for '{}@{}' does not match the uploaded artifact",
                artifact.name, artifact.version
            ),
            Span::default(),
        ));
    }
    if normalized_registry_dependencies(&committed.dependencies)
        != normalized_registry_dependencies(dependencies)
    {
        return Err(KuError::package(
            "publish_verification_failed",
            format!(
                "signed registry index dependencies for '{}@{}' do not match ku.mod",
                artifact.name, artifact.version
            ),
            Span::default(),
        ));
    }
    Ok(())
}

fn validate_publish_token(token: &str, span: Span) -> KuResult<()> {
    if token.is_empty()
        || token.len() > MAX_PUBLISH_TOKEN_BYTES
        || !token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
    {
        return Err(KuError::package(
            "invalid_publish_token",
            format!(
                "{REGISTRY_TOKEN_ENV} must be a non-empty ASCII bearer token of at most {MAX_PUBLISH_TOKEN_BYTES} bytes"
            ),
            span,
        ));
    }
    Ok(())
}

fn validate_publish_http_status(status: u16, artifact: &PackageArtifact) -> KuResult<()> {
    match status {
        200 | 201 | 204 => Ok(()),
        409 => Err(KuError::package(
            "package_version_conflict",
            format!(
                "registry already contains different content for '{}@{}'",
                artifact.name, artifact.version
            ),
            Span::default(),
        )),
        401 | 403 => Err(KuError::package(
            "registry_publish_unauthorized",
            format!("registry rejected publish credentials with HTTP status {status}"),
            Span::default(),
        )),
        _ => Err(KuError::package(
            "registry_publish_status",
            format!("registry returned publish HTTP status {status}"),
            Span::default(),
        )),
    }
}

fn collect_package_archive_entries(
    package: &PackageContext,
    policy: PackageArchivePolicy,
    span: Span,
) -> KuResult<CollectedPackageArchive> {
    let canonical_root = fs::canonicalize(&package.package_dir).map_err(|err| {
        KuError::message(format!(
            "failed to resolve package directory '{}': {err}",
            package.package_dir.display()
        ))
    })?;
    let source_root = ArchiveSourceRoot::open(canonical_root, span)?;
    let canonical_cache = fs::canonicalize(&package.cache_dir).ok();
    let mut output = Vec::new();
    // The deterministic tar writer adds one synthetic package root directory.
    let mut entries_seen = 1usize;
    let mut bytes = 0u64;
    for name in [
        MANIFEST_FILE,
        DEFAULT_IMPORT_ROOT,
        "README",
        "README.md",
        "LICENSE",
        "LICENSE.md",
        "docs",
        "examples",
        "tests",
    ] {
        let path = package.package_dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => collect_package_archive_entry(
                &source_root,
                canonical_cache.as_deref(),
                &path,
                &mut output,
                &mut entries_seen,
                &mut bytes,
                policy,
                span,
            )?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to inspect package path '{}': {err}",
                    path.display()
                )));
            }
        }
    }
    if !output.iter().any(|entry| entry.relative == MANIFEST_FILE) {
        return Err(KuError::package(
            "missing_package_manifest",
            "package root must contain ku.mod",
            span,
        ));
    }
    if !output
        .iter()
        .any(|entry| entry.relative == DEFAULT_IMPORT_ROOT && entry.is_dir)
    {
        return Err(KuError::package(
            "missing_package_root",
            "published package must contain the src directory",
            span,
        ));
    }
    output.sort_by(|left, right| left.relative.cmp(&right.relative));
    let mut portable_paths = HashSet::new();
    for entry in &output {
        if !portable_paths.insert(entry.relative.to_lowercase()) {
            return Err(KuError::package(
                "duplicate_package_path",
                format!(
                    "package contains case-conflicting path '{}'",
                    entry.relative
                ),
                span,
            ));
        }
    }
    Ok(CollectedPackageArchive {
        source_root,
        entries: output,
    })
}

fn package_publish_path_is_excluded(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            matches!(
                lower.as_str(),
                ".git" | ".hg" | ".svn" | ".ku" | "ku.lock" | "target" | "node_modules"
            ) || lower == ".env"
                || lower.starts_with(".env.")
        })
}

fn package_publish_file_is_forbidden(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "install" | "preinstall" | "postinstall" | "build"
    ) {
        return true;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "exe"
                    | "dll"
                    | "so"
                    | "dylib"
                    | "msi"
                    | "com"
                    | "scr"
                    | "bat"
                    | "cmd"
                    | "ps1"
                    | "sh"
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn collect_package_archive_entry(
    source_root: &ArchiveSourceRoot,
    canonical_cache: Option<&Path>,
    path: &Path,
    output: &mut Vec<ArchiveSourceEntry>,
    files: &mut usize,
    bytes: &mut u64,
    policy: PackageArchivePolicy,
    span: Span,
) -> KuResult<()> {
    *files += 1;
    if *files > policy.max_files {
        return Err(KuError::package(
            "package_archive_limit",
            "published package exceeds entry count limit",
            span,
        ));
    }
    if package_publish_path_is_excluded(path) {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        KuError::message(format!(
            "failed to read package metadata '{}': {err}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(KuError::package(
            "unsupported_symlink",
            format!("package contains unsupported symlink '{}'", path.display()),
            span,
        ));
    }
    if !(metadata.is_dir() || metadata.is_file()) {
        return Err(KuError::package(
            "unsupported_package_entry",
            format!(
                "package contains unsupported filesystem entry '{}'",
                path.display()
            ),
            span,
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|err| {
        KuError::message(format!(
            "failed to resolve package path '{}': {err}",
            path.display()
        ))
    })?;
    if !canonical.starts_with(&source_root.canonical) {
        return Err(KuError::package(
            "unsafe_package_path",
            format!("package path escapes package root '{}'", path.display()),
            span,
        ));
    }
    if canonical_cache.is_some_and(|cache| canonical.starts_with(cache)) {
        return Ok(());
    }
    let relative_path = path
        .strip_prefix(&source_root.canonical)
        .or_else(|_| canonical.strip_prefix(&source_root.canonical))
        .map_err(|_| {
            KuError::package(
                "unsafe_package_path",
                format!("failed to compute package path '{}'", path.display()),
                span,
            )
        })?;
    let relative = relative_path
        .to_str()
        .ok_or_else(|| {
            KuError::package(
                "unsafe_package_path",
                "published package paths must be valid UTF-8",
                span,
            )
        })?
        .replace('\\', "/");
    if relative.is_empty()
        || relative.len() > policy.max_path_bytes
        || relative.split('/').count() + 1 > policy.max_depth
    {
        return Err(KuError::package(
            "unsafe_package_path",
            format!("published package path is too long or too deep: '{relative}'"),
            span,
        ));
    }
    for component in relative.split('/') {
        validate_portable_package_component(component, span)?;
    }
    if metadata.is_dir() {
        output.push(ArchiveSourceEntry {
            relative,
            is_dir: true,
            size: 0,
            identity: None,
        });
        let remaining = policy.max_files.saturating_sub(*files);
        let mut children = Vec::with_capacity(remaining.min(64));
        for child in fs::read_dir(path).map_err(|err| {
            KuError::message(format!(
                "failed to read package directory '{}': {err}",
                path.display()
            ))
        })? {
            if children.len() >= remaining {
                return Err(KuError::package(
                    "package_archive_limit",
                    "published package exceeds entry count limit",
                    span,
                ));
            }
            children.push(
                child.map_err(|err| {
                    KuError::message(format!("failed to read package entry: {err}"))
                })?,
            );
        }
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            collect_package_archive_entry(
                source_root,
                canonical_cache,
                &child.path(),
                output,
                files,
                bytes,
                policy,
                span,
            )?;
        }
    } else {
        if package_publish_file_is_forbidden(path) {
            return Err(KuError::package(
                "unsupported_package_entry",
                format!(
                    "published package cannot contain executable or install script '{}'",
                    relative
                ),
                span,
            ));
        }
        let file = open_archive_source_file(source_root, &relative, span)?;
        let opened_metadata = file.metadata().map_err(|err| {
            archive_source_changed_error(
                &relative,
                format!("failed to inspect opened package file: {err}"),
                span,
            )
        })?;
        if !opened_metadata.is_file() || opened_metadata.len() != metadata.len() {
            return Err(archive_source_changed_error(
                &relative,
                "package file identity or size changed while it was being enumerated",
                span,
            ));
        }
        let identity = archive_file_identity(&file, span, &relative)?;
        *bytes = bytes.saturating_add(opened_metadata.len());
        if opened_metadata.len() > policy.max_file_bytes || *bytes > policy.max_unpacked_bytes {
            return Err(KuError::package(
                "package_archive_limit",
                "published package exceeds file count or size limits",
                span,
            ));
        }
        output.push(ArchiveSourceEntry {
            relative,
            is_dir: false,
            size: opened_metadata.len(),
            identity: Some(identity),
        });
    }
    Ok(())
}

impl ArchiveSourceRoot {
    fn open(canonical: PathBuf, span: Span) -> KuResult<Self> {
        let handle = open_archive_source_root(&canonical).map_err(|err| {
            KuError::package(
                "unsafe_package_path",
                format!(
                    "failed to securely open package root '{}': {err}",
                    canonical.display()
                ),
                span,
            )
        })?;
        let metadata = handle.metadata().map_err(|err| {
            KuError::package(
                "unsafe_package_path",
                format!(
                    "failed to inspect opened package root '{}': {err}",
                    canonical.display()
                ),
                span,
            )
        })?;
        if !metadata.is_dir() || archive_metadata_is_reparse_point(&metadata) {
            return Err(KuError::package(
                "unsafe_package_path",
                "package root must be a real directory",
                span,
            ));
        }
        #[cfg(windows)]
        let final_path = windows_final_path(&handle).map_err(|err| {
            KuError::package(
                "unsafe_package_path",
                format!("failed to resolve the opened package root: {err}"),
                span,
            )
        })?;
        Ok(Self {
            canonical,
            _handle: handle,
            #[cfg(windows)]
            final_path,
        })
    }
}

fn archive_source_changed_error(relative: &str, detail: impl AsRef<str>, span: Span) -> KuError {
    KuError::package(
        "package_source_changed",
        format!(
            "package source '{}' changed while it was being packed; retry the command: {}",
            relative,
            detail.as_ref()
        ),
        span,
    )
}

fn validate_open_archive_source(
    file: &fs::File,
    entry: &ArchiveSourceEntry,
    span: Span,
) -> KuResult<()> {
    let metadata = file.metadata().map_err(|err| {
        archive_source_changed_error(
            &entry.relative,
            format!("failed to inspect the opened file: {err}"),
            span,
        )
    })?;
    if !metadata.is_file()
        || archive_metadata_is_reparse_point(&metadata)
        || metadata.len() != entry.size
        || entry.identity != Some(archive_file_identity(file, span, &entry.relative)?)
    {
        return Err(archive_source_changed_error(
            &entry.relative,
            "file identity, type, or size no longer matches the enumerated source",
            span,
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn archive_file_identity(
    file: &fs::File,
    span: Span,
    relative: &str,
) -> KuResult<ArchiveFileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|err| {
        archive_source_changed_error(
            relative,
            format!("failed to read file identity: {err}"),
            span,
        )
    })?;
    Ok(ArchiveFileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
fn archive_file_identity(
    file: &fs::File,
    span: Span,
    relative: &str,
) -> KuResult<ArchiveFileIdentity> {
    windows_file_identity(file).map_err(|err| {
        archive_source_changed_error(
            relative,
            format!("failed to read file identity: {err}"),
            span,
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn archive_file_identity(
    file: &fs::File,
    span: Span,
    relative: &str,
) -> KuResult<ArchiveFileIdentity> {
    let metadata = file.metadata().map_err(|err| {
        archive_source_changed_error(
            relative,
            format!("failed to read file identity: {err}"),
            span,
        )
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos() as u64);
    Ok(ArchiveFileIdentity {
        volume: metadata.len(),
        file: modified,
        modified_seconds: modified as i64,
        modified_nanos: 0,
        changed_seconds: 0,
        changed_nanos: 0,
    })
}

#[cfg(not(windows))]
fn archive_metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn archive_metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn open_archive_source_root(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY)
        .open(path)
}

#[cfg(windows)]
fn open_archive_source_root(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(KU_FILE_SHARE_ALL)
        .custom_flags(KU_FILE_FLAG_BACKUP_SEMANTICS | KU_FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_archive_source_root(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

#[cfg(unix)]
fn open_archive_source_file(
    root: &ArchiveSourceRoot,
    relative: &str,
    span: Span,
) -> KuResult<fs::File> {
    use std::{
        ffi::CString,
        os::{fd::AsRawFd, fd::FromRawFd},
    };

    let components = relative.split('/').collect::<Vec<_>>();
    let Some((file_name, directories)) = components.split_last() else {
        return Err(archive_source_changed_error(
            relative,
            "source path has no file component",
            span,
        ));
    };
    let mut directory = root._handle.try_clone().map_err(|err| {
        archive_source_changed_error(
            relative,
            format!("failed to clone root handle: {err}"),
            span,
        )
    })?;
    for component in directories {
        let component = CString::new(*component).map_err(|_| {
            archive_source_changed_error(relative, "source path contains a NUL byte", span)
        })?;
        // SAFETY: `directory` owns a live directory fd, `component` is NUL
        // terminated, and no creation mode argument is required by these flags.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY,
            )
        };
        if descriptor < 0 {
            return Err(archive_source_changed_error(
                relative,
                format!(
                    "failed to open a package directory without following links: {}",
                    io::Error::last_os_error()
                ),
                span,
            ));
        }
        // SAFETY: `openat` returned a new owned descriptor on success.
        let opened = unsafe { fs::File::from_raw_fd(descriptor) };
        let metadata = opened.metadata().map_err(|err| {
            archive_source_changed_error(
                relative,
                format!("failed to inspect an opened package directory: {err}"),
                span,
            )
        })?;
        if !metadata.is_dir() {
            return Err(archive_source_changed_error(
                relative,
                "a package path component is no longer a directory",
                span,
            ));
        }
        directory = opened;
    }
    let file_name = CString::new(*file_name).map_err(|_| {
        archive_source_changed_error(relative, "source path contains a NUL byte", span)
    })?;
    // SAFETY: `directory` owns a live directory fd, `file_name` is NUL
    // terminated, and no creation mode argument is required by these flags.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(archive_source_changed_error(
            relative,
            format!(
                "failed to open package file without following links: {}",
                io::Error::last_os_error()
            ),
            span,
        ));
    }
    // SAFETY: `openat` returned a new owned descriptor on success.
    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}

#[cfg(windows)]
fn open_archive_source_file(
    root: &ArchiveSourceRoot,
    relative: &str,
    span: Span,
) -> KuResult<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(KU_FILE_SHARE_READ)
        .custom_flags(KU_FILE_FLAG_OPEN_REPARSE_POINT)
        .open(root.canonical.join(relative.replace('/', "\\")))
        .map_err(|err| {
            archive_source_changed_error(
                relative,
                format!("failed to open package file without following reparse points: {err}"),
                span,
            )
        })?;
    let metadata = file.metadata().map_err(|err| {
        archive_source_changed_error(
            relative,
            format!("failed to inspect opened package file: {err}"),
            span,
        )
    })?;
    if archive_metadata_is_reparse_point(&metadata) {
        return Err(archive_source_changed_error(
            relative,
            "package file was replaced by a reparse point",
            span,
        ));
    }
    let final_path = windows_final_path(&file).map_err(|err| {
        archive_source_changed_error(
            relative,
            format!("failed to resolve opened package file: {err}"),
            span,
        )
    })?;
    if !windows_path_is_strict_child(&root.final_path, &final_path) {
        return Err(KuError::package(
            "unsafe_package_path",
            format!("opened package file '{relative}' escaped the package root"),
            span,
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_archive_source_file(
    root: &ArchiveSourceRoot,
    relative: &str,
    span: Span,
) -> KuResult<fs::File> {
    let path = root.canonical.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|err| {
        archive_source_changed_error(relative, format!("failed to inspect source: {err}"), span)
    })?;
    if metadata.file_type().is_symlink() {
        return Err(archive_source_changed_error(
            relative,
            "package file was replaced by a symbolic link",
            span,
        ));
    }
    let canonical = fs::canonicalize(&path).map_err(|err| {
        archive_source_changed_error(relative, format!("failed to resolve source: {err}"), span)
    })?;
    if !canonical.starts_with(&root.canonical) {
        return Err(KuError::package(
            "unsafe_package_path",
            format!("opened package file '{relative}' escaped the package root"),
            span,
        ));
    }
    fs::File::open(canonical).map_err(|err| {
        archive_source_changed_error(relative, format!("failed to open source: {err}"), span)
    })
}

#[cfg(windows)]
const KU_FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const KU_FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const KU_FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const KU_FILE_SHARE_ALL: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;

#[cfg(windows)]
#[repr(C)]
struct KuWindowsFileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[repr(C)]
struct KuWindowsFileInformation {
    attributes: u32,
    creation_time: KuWindowsFileTime,
    last_access_time: KuWindowsFileTime,
    last_write_time: KuWindowsFileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
fn windows_file_identity(file: &fs::File) -> io::Result<ArchiveFileIdentity> {
    use std::{
        mem::MaybeUninit,
        os::windows::io::{AsRawHandle, RawHandle},
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            file: RawHandle,
            information: *mut KuWindowsFileInformation,
        ) -> i32;
    }

    let mut information = MaybeUninit::<KuWindowsFileInformation>::uninit();
    // SAFETY: the OS writes the full information structure on success and the
    // raw handle remains owned by `file` for the duration of the call.
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a nonzero result guarantees that the structure was initialized.
    let information = unsafe { information.assume_init() };
    let last_write = (u64::from(information.last_write_time.high) << 32)
        | u64::from(information.last_write_time.low);
    Ok(ArchiveFileIdentity {
        volume: u64::from(information.volume_serial_number),
        file: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
        modified_seconds: last_write as i64,
        modified_nanos: 0,
        changed_seconds: 0,
        changed_nanos: 0,
    })
}

#[cfg(windows)]
fn windows_final_path(file: &fs::File) -> io::Result<String> {
    use std::os::windows::io::{AsRawHandle, RawHandle};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            file: RawHandle,
            path: *mut u16,
            path_len: u32,
            flags: u32,
        ) -> u32;
    }

    // Windows extended-length paths are bounded at 32,767 UTF-16 code units.
    // A fixed buffer avoids an attacker-controlled allocation/retry loop.
    let mut buffer = vec![0u16; 32_768];
    // SAFETY: `buffer` is writable for its declared length and the raw handle
    // remains owned by `file` for the duration of the call.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize >= buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resolved Windows path exceeds the supported path limit",
        ));
    }
    String::from_utf16(&buffer[..length as usize])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "resolved path is not UTF-16"))
}

#[cfg(windows)]
fn windows_path_is_strict_child(root: &str, child: &str) -> bool {
    // Both paths come from GetFinalPathNameByHandleW. Compare that normalized
    // spelling exactly instead of applying Rust's Unicode case folding, which
    // is not the same operation as Windows filesystem name comparison. A
    // spelling mismatch therefore fails closed.
    let root = root.trim_end_matches(['\\', '/']);
    let child = child.replace('/', "\\");
    child
        .strip_prefix(root)
        .is_some_and(|suffix| suffix.starts_with('\\') && suffix.len() > 1)
}

fn write_deterministic_package_archive(
    path: &Path,
    name: &str,
    version: &str,
    collected: &CollectedPackageArchive,
    max_compressed_bytes: u64,
    span: Span,
) -> KuResult<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| {
            KuError::message(format!(
                "failed to create package artifact '{}': {err}",
                path.display()
            ))
        })?;
    let compressed_limit_exceeded = Arc::new(AtomicBool::new(false));
    let writer = CompressedLimitWriter::new(
        file,
        max_compressed_bytes,
        Arc::clone(&compressed_limit_exceeded),
    );
    let mut encoder = zstd::stream::write::Encoder::new(writer, 3).map_err(|err| {
        package_archive_write_error(
            err,
            &compressed_limit_exceeded,
            max_compressed_bytes,
            "failed to create zstd package encoder",
            span,
        )
    })?;
    encoder.include_checksum(true).map_err(|err| {
        package_archive_write_error(
            err,
            &compressed_limit_exceeded,
            max_compressed_bytes,
            "failed to configure zstd package encoder",
            span,
        )
    })?;
    let mut builder = tar::Builder::new(encoder);
    let archive_root = format!("{name}-{version}");
    let policy = PackageArchivePolicy::default();
    for entry in &collected.entries {
        let archive_path = format!("{archive_root}/{}", entry.relative);
        if archive_path.len() > policy.max_path_bytes
            || archive_path.split('/').count() > policy.max_depth
        {
            return Err(KuError::package(
                "unsafe_package_path",
                format!("published package archive path is too long or too deep: '{archive_path}'"),
                span,
            ));
        }
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        if entry.is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_data(&mut header, archive_path, io::empty())
                .map_err(|err| {
                    package_archive_write_error(
                        err,
                        &compressed_limit_exceeded,
                        max_compressed_bytes,
                        "failed to append package directory",
                        span,
                    )
                })?;
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(entry.size);
            header.set_cksum();
            let mut file = open_archive_source_file(&collected.source_root, &entry.relative, span)?;
            validate_open_archive_source(&file, entry, span)?;
            builder
                .append_data(&mut header, archive_path, &mut file)
                .map_err(|err| {
                    package_archive_write_error(
                        err,
                        &compressed_limit_exceeded,
                        max_compressed_bytes,
                        &format!("failed to append package file '{}'", entry.relative),
                        span,
                    )
                })?;
            validate_open_archive_source(&file, entry, span)?;
        }
    }
    let encoder = builder.into_inner().map_err(|err| {
        package_archive_write_error(
            err,
            &compressed_limit_exceeded,
            max_compressed_bytes,
            "failed to finish tar package",
            span,
        )
    })?;
    let writer = encoder.finish().map_err(|err| {
        package_archive_write_error(
            err,
            &compressed_limit_exceeded,
            max_compressed_bytes,
            "failed to finish zstd package",
            span,
        )
    })?;
    let file = writer.into_inner();
    file.sync_all().map_err(|err| {
        KuError::message(format!(
            "failed to sync package artifact '{}': {err}",
            path.display()
        ))
    })
}

struct CompressedLimitWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
    exceeded: Arc<AtomicBool>,
}

impl<W> CompressedLimitWriter<W> {
    fn new(inner: W, limit: u64, exceeded: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            written: 0,
            limit,
            exceeded,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CompressedLimitWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() as u64 > self.limit.saturating_sub(self.written) {
            self.exceeded.store(true, Ordering::Relaxed);
            return Err(io::Error::other("compressed package byte limit exceeded"));
        }
        let written = self.inner.write(buffer)?;
        self.written = self.written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn package_archive_write_error(
    err: io::Error,
    exceeded: &AtomicBool,
    max_compressed_bytes: u64,
    context: &str,
    span: Span,
) -> KuError {
    if exceeded.load(Ordering::Relaxed) {
        KuError::package(
            "package_archive_limit",
            format!("package artifact exceeds compressed limit of {max_compressed_bytes} bytes"),
            span,
        )
    } else {
        KuError::package("package_archive_failed", format!("{context}: {err}"), span)
    }
}

fn sha256_file(path: &Path, max_bytes: u64) -> KuResult<String> {
    let file = fs::File::open(path).map_err(|err| {
        KuError::message(format!(
            "failed to open package artifact '{}': {err}",
            path.display()
        ))
    })?;
    stream_sha256(file, io::sink(), max_bytes)
}

struct TemporaryFile {
    path: PathBuf,
    remove_on_drop: bool,
}

impl TemporaryFile {
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

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackStagingKind {
    Archive,
    Verification,
}

fn operation_nonce_is_valid(value: &str) -> bool {
    let mut parts = value.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(process), Some(counter), Some(nanos), None)
            if process.parse::<u32>().is_ok()
                && counter.parse::<u64>().is_ok()
                && nanos.parse::<u128>().is_ok()
    )
}

fn pack_staging_kind(file_name: &str) -> Option<PackStagingKind> {
    let value = file_name.strip_prefix('.')?;
    let (stem, kind) = if let Some(stem) = value.strip_suffix(".tar.zst.tmp") {
        (stem, PackStagingKind::Archive)
    } else if let Some(stem) = value.strip_suffix(".verify") {
        (stem, PackStagingKind::Verification)
    } else {
        return None;
    };
    let mut nonce_parts = stem.rsplitn(4, '-');
    let nanos = nonce_parts.next()?;
    let counter = nonce_parts.next()?;
    let process = nonce_parts.next()?;
    let package_version = nonce_parts.next()?;
    if process.parse::<u32>().is_err()
        || counter.parse::<u64>().is_err()
        || nanos.parse::<u128>().is_err()
    {
        return None;
    }
    let (package_name, version) = package_version.rsplit_once('-')?;
    (validate_package_name(package_name, Span::default()).is_ok()
        && parse_package_version(version, Span::default()).is_ok())
    .then_some(kind)
}

fn metadata_is_stale_after(metadata: &fs::Metadata, stale_after: Duration) -> bool {
    metadata
        .modified()
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
        .is_ok_and(|age| age >= stale_after)
}

struct PackStagingCleanupBudget {
    remaining_nested_entries: usize,
    deadline: Instant,
}

impl PackStagingCleanupBudget {
    fn new(max_nested_entries: usize, max_duration: Duration) -> Self {
        Self {
            remaining_nested_entries: max_nested_entries,
            deadline: Instant::now() + max_duration,
        }
    }

    fn consume_nested_entry(&mut self) -> bool {
        if !self.is_active() {
            return false;
        }
        self.remaining_nested_entries -= 1;
        true
    }

    fn is_active(&self) -> bool {
        self.remaining_nested_entries > 0 && Instant::now() < self.deadline
    }

    fn before_deadline(&self) -> bool {
        Instant::now() < self.deadline
    }
}

fn verification_staging_tree_is_bounded_and_plain(
    root: &Path,
    policy: PackageArchivePolicy,
    cleanup_budget: &mut PackStagingCleanupBudget,
) -> io::Result<bool> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut entries_seen = 0usize;
    let mut bytes_seen = 0u64;
    while let Some((directory, depth)) = stack.pop() {
        if depth > policy.max_depth {
            return Ok(false);
        }
        for entry in fs::read_dir(directory)? {
            if !cleanup_budget.consume_nested_entry() {
                return Ok(false);
            }
            let entry = entry?;
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > policy.max_files {
                return Ok(false);
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Ok(false);
            }
            if metadata.is_dir() {
                stack.push((entry.path(), depth.saturating_add(1)));
            } else if metadata.is_file() {
                if metadata.len() > policy.max_file_bytes {
                    return Ok(false);
                }
                bytes_seen = bytes_seen.saturating_add(metadata.len());
                if bytes_seen > policy.max_unpacked_bytes {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn cleanup_stale_pack_staging(
    distribution_dir: &Path,
    stale_after: Duration,
    max_scan_entries: usize,
    policy: PackageArchivePolicy,
) -> KuResult<usize> {
    if max_scan_entries == 0 {
        return Ok(0);
    }
    let distribution_metadata = fs::symlink_metadata(distribution_dir).map_err(|err| {
        KuError::message(format!(
            "failed to inspect package artifact directory '{}': {err}",
            distribution_dir.display()
        ))
    })?;
    if distribution_metadata.file_type().is_symlink() || !distribution_metadata.is_dir() {
        return Err(KuError::package(
            "unsafe_artifact_path",
            "package artifact directory must be a real directory",
            Span::default(),
        ));
    }
    let canonical_distribution = fs::canonicalize(distribution_dir).map_err(|err| {
        KuError::message(format!(
            "failed to resolve package artifact directory '{}': {err}",
            distribution_dir.display()
        ))
    })?;
    let mut removed = 0usize;
    let mut cleanup_budget = PackStagingCleanupBudget::new(
        MAX_PACK_STAGING_NESTED_ENTRIES,
        Duration::from_secs(CACHE_GC_DEADLINE_SECS),
    );
    for (scanned, entry) in fs::read_dir(distribution_dir)
        .map_err(|err| {
            KuError::message(format!(
                "failed to read package artifact directory '{}': {err}",
                distribution_dir.display()
            ))
        })?
        .enumerate()
    {
        if scanned >= max_scan_entries || !cleanup_budget.before_deadline() {
            break;
        }
        let entry = entry.map_err(|err| {
            KuError::message(format!("failed to read package artifact entry: {err}"))
        })?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(kind) = pack_staging_kind(&file_name) else {
            continue;
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to inspect package staging '{}': {err}",
                    path.display()
                )));
            }
        };
        if metadata.file_type().is_symlink()
            || !metadata_is_stale_after(&metadata, stale_after)
            || (kind == PackStagingKind::Archive && !metadata.is_file())
            || (kind == PackStagingKind::Verification && !metadata.is_dir())
        {
            continue;
        }
        let canonical_path = match fs::canonicalize(&path) {
            Ok(path) => path,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to resolve package staging '{}': {err}",
                    path.display()
                )));
            }
        };
        if canonical_path.parent() != Some(canonical_distribution.as_path()) {
            continue;
        }
        if kind == PackStagingKind::Verification
            && !verification_staging_tree_is_bounded_and_plain(
                &canonical_path,
                policy,
                &mut cleanup_budget,
            )
            .map_err(|err| {
                KuError::message(format!(
                    "failed to inspect package verification staging '{}': {err}",
                    canonical_path.display()
                ))
            })?
        {
            if !cleanup_budget.is_active() {
                break;
            }
            continue;
        }
        let current_metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to re-inspect package staging '{}': {err}",
                    path.display()
                )));
            }
        };
        if current_metadata.file_type().is_symlink()
            || !metadata_is_stale_after(&current_metadata, stale_after)
            || (kind == PackStagingKind::Archive && !current_metadata.is_file())
            || (kind == PackStagingKind::Verification && !current_metadata.is_dir())
        {
            continue;
        }
        let result = match kind {
            PackStagingKind::Archive => fs::remove_file(&path),
            PackStagingKind::Verification => fs::remove_dir_all(&path),
        };
        match result {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to remove stale package staging '{}': {err}",
                    path.display()
                )));
            }
        }
    }
    Ok(removed)
}

fn package_lock_staging_name_is_valid(file_name: &str, lock_file_name: &str) -> bool {
    file_name
        .strip_prefix(&format!("{lock_file_name}."))
        .and_then(|value| value.strip_suffix(".tmp"))
        .is_some_and(operation_nonce_is_valid)
}

fn cleanup_stale_package_lock_staging(
    lock_path: &Path,
    stale_after: Duration,
    max_scan_entries: usize,
) -> KuResult<usize> {
    if max_scan_entries == 0 {
        return Ok(0);
    }
    let parent = lock_path.parent().unwrap_or_else(|| Path::new("."));
    let lock_file_name = lock_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            KuError::package(
                "unsafe_package_lock",
                "package lock path must have a valid UTF-8 file name",
                Span::default(),
            )
        })?;
    let mut removed = 0usize;
    for (scanned, entry) in fs::read_dir(parent)
        .map_err(|err| {
            KuError::message(format!(
                "failed to read package lock directory '{}': {err}",
                parent.display()
            ))
        })?
        .enumerate()
    {
        if scanned >= max_scan_entries {
            break;
        }
        let entry = entry
            .map_err(|err| KuError::message(format!("failed to read package lock entry: {err}")))?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !package_lock_staging_name_is_valid(&file_name, lock_file_name) {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to inspect package lock staging '{}': {err}",
                    path.display()
                )));
            }
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || !metadata_is_stale_after(&metadata, stale_after)
        {
            continue;
        }
        let current_metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to re-inspect package lock staging '{}': {err}",
                    path.display()
                )));
            }
        };
        if current_metadata.file_type().is_symlink()
            || !current_metadata.is_file()
            || !metadata_is_stale_after(&current_metadata, stale_after)
        {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(KuError::message(format!(
                    "failed to remove stale package lock staging '{}': {err}",
                    path.display()
                )));
            }
        }
    }
    Ok(removed)
}

fn registry_quarantine_tree_is_bounded_and_plain(
    root: &Path,
    scan_budget: &mut CacheGcScanBudget,
) -> io::Result<bool> {
    let policy = PackageArchivePolicy::default();
    let max_total_bytes = policy
        .max_unpacked_bytes
        .saturating_add(MAX_REGISTRY_DOWNLOAD_BYTES);
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut entries_seen = 0usize;
    let mut bytes_seen = 0u64;
    while let Some((directory, depth)) = stack.pop() {
        if depth > policy.max_depth || Instant::now() >= scan_budget.deadline {
            return Ok(false);
        }
        for entry in fs::read_dir(directory)? {
            if !scan_budget.consume() {
                return Ok(false);
            }
            let entry = entry?;
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > policy.max_files.saturating_add(4) {
                return Ok(false);
            }
            let path = entry.path();
            let relative = match path.strip_prefix(root).ok().and_then(Path::to_str) {
                Some(relative) if relative.len() <= policy.max_path_bytes => relative,
                _ => return Ok(false),
            };
            if relative.split(['/', '\\']).count().saturating_add(1) > policy.max_depth {
                return Ok(false);
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || archive_metadata_is_reparse_point(&metadata) {
                return Ok(false);
            }
            if metadata.is_dir() {
                stack.push((path, depth.saturating_add(1)));
            } else if metadata.is_file() {
                if metadata.len() > MAX_REGISTRY_DOWNLOAD_BYTES {
                    return Ok(false);
                }
                bytes_seen = bytes_seen.saturating_add(metadata.len());
                if bytes_seen > max_total_bytes {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn cleanup_registry_quarantine_candidate(
    cache_dir: &Path,
    canonical_packages_dir: &Path,
    name_path: &Path,
    path: &Path,
    stale_after: Duration,
    scan_budget: &mut CacheGcScanBudget,
) -> KuResult<bool> {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    if !registry_quarantine_name_is_valid(file_name) {
        return Ok(false);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to inspect registry cache quarantine '{}': {err}",
                path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink()
        || archive_metadata_is_reparse_point(&metadata)
        || !metadata.is_dir()
        || !metadata_is_stale_after(&metadata, stale_after)
    {
        return Ok(false);
    }
    let Some(package_name) = name_path.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    if validate_package_name(package_name, Span::default()).is_err() {
        return Ok(false);
    }
    let name_directory = open_registry_cache_directory(
        name_path,
        "registry quarantine package root",
        Span::default(),
    )?;
    if name_directory.canonical.parent() != Some(canonical_packages_dir) {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry quarantine package root escaped the packages cache",
            Span::default(),
        ));
    }
    let quarantine =
        open_registry_cache_directory(path, "registry cache quarantine", Span::default())?;
    if quarantine.canonical.parent() != Some(name_directory.canonical.as_path()) {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry cache quarantine escaped its exact package root",
            Span::default(),
        ));
    }
    let quarantine_lock = try_acquire_registry_install_lock(&package_operation_lock_path(
        cache_dir,
        "registry-quarantine",
        package_name,
        file_name,
    ))?;
    let Some(_quarantine_lock) = quarantine_lock else {
        return Ok(false);
    };
    if !registry_quarantine_tree_is_bounded_and_plain(path, scan_budget).map_err(|err| {
        KuError::message(format!(
            "failed to inspect registry cache quarantine '{}': {err}",
            path.display()
        ))
    })? {
        return Ok(false);
    }
    let current =
        open_registry_cache_directory(path, "registry cache quarantine", Span::default())?;
    if current.canonical != quarantine.canonical
        || current.identity != quarantine.identity
        || !fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata_is_stale_after(&metadata, stale_after))
    {
        return Ok(false);
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(KuError::message(format!(
            "failed to remove bounded registry cache quarantine '{}': {err}",
            path.display()
        ))),
    }
}

pub fn gc_cache(package: &PackageContext, max_entries: usize) -> KuResult<usize> {
    ensure_cache_dir(package)?;
    let _usage_lock = acquire_registry_install_lock(
        &package.cache_dir.join(".package-usage.lock"),
        Span::default(),
    )?;
    if max_entries == 0 {
        return Ok(0);
    }
    let mut scan_budget = CacheGcScanBudget::new(max_entries);
    cleanup_package_operation_lock_files(package, max_entries.saturating_mul(2), &mut scan_budget)?;
    let packages_dir = package.cache_dir.join(PACKAGE_CACHE_DIR);
    if !packages_dir.exists() {
        return gc_stale_registry_staging(package, max_entries, &mut scan_budget);
    }
    let canonical_packages_dir = fs::canonicalize(&packages_dir).map_err(|err| {
        KuError::message(format!(
            "failed to resolve package cache '{}': {err}",
            packages_dir.display()
        ))
    })?;
    let keep = gc_locked_cache_keep_set(package, &mut scan_budget)?;
    let mut removed = 0;
    for name_entry in fs::read_dir(&packages_dir).map_err(|err| {
        KuError::message(format!(
            "failed to read package cache '{}': {err}",
            packages_dir.display()
        ))
    })? {
        if !scan_budget.consume() {
            break;
        }
        let name_entry = name_entry
            .map_err(|err| KuError::message(format!("failed to read cache entry: {err}")))?;
        let name_path = name_entry.path();
        let name_metadata = fs::symlink_metadata(&name_path).map_err(|err| {
            KuError::message(format!(
                "failed to inspect package cache '{}': {err}",
                name_path.display()
            ))
        })?;
        if name_metadata.file_type().is_symlink() || !name_metadata.is_dir() {
            continue;
        }
        for version_entry in fs::read_dir(&name_path).map_err(|err| {
            KuError::message(format!(
                "failed to read package cache '{}': {err}",
                name_path.display()
            ))
        })? {
            if !scan_budget.consume() {
                break;
            }
            let version_entry = version_entry
                .map_err(|err| KuError::message(format!("failed to read cache entry: {err}")))?;
            let path = version_entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|err| {
                KuError::message(format!(
                    "failed to inspect package cache '{}': {err}",
                    path.display()
                ))
            })?;
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let package_name = name_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if path.file_name().is_some_and(|name| {
                name.as_encoded_bytes()
                    .starts_with(REGISTRY_QUARANTINE_PREFIX.as_bytes())
            }) {
                // Every reserved-prefix entry stays out of generic GC. An
                // unknown name is not proof that Ku owns this directory, and
                // must not bypass the quarantine age/tree/identity checks.
                if registry_quarantine_name_is_valid(file_name)
                    && removed < max_entries
                    && cleanup_registry_quarantine_candidate(
                        &package.cache_dir,
                        &canonical_packages_dir,
                        &name_path,
                        &path,
                        Duration::from_secs(REGISTRY_STAGING_STALE_SECS),
                        &mut scan_budget,
                    )?
                {
                    removed += 1;
                }
                continue;
            }
            if !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && file_name.contains(".file-copy")
            {
                if removed >= max_entries || !cache_entry_is_stale(&metadata) {
                    continue;
                }
                let canonical_path = fs::canonicalize(&path).map_err(|err| {
                    KuError::message(format!(
                        "failed to resolve file dependency staging '{}': {err}",
                        path.display()
                    ))
                })?;
                if !canonical_path.starts_with(&canonical_packages_dir)
                    || canonical_path == canonical_packages_dir
                {
                    return Err(KuError::package(
                        "unsafe_cache_path",
                        "refusing to remove file dependency staging outside its cache root",
                        Span::default(),
                    ));
                }
                fs::remove_dir_all(&canonical_path).map_err(|err| {
                    KuError::message(format!(
                        "failed to remove stale file dependency staging '{}': {err}",
                        canonical_path.display()
                    ))
                })?;
                removed += 1;
                continue;
            }
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || keep.contains(&path)
                || file_name.contains(".download-")
                || file_name.contains(".replaced-")
                || file_name.contains(".file-copy")
            {
                continue;
            }
            let registry_delete_lock =
                try_acquire_registry_install_lock(&package_operation_lock_path(
                    &package.cache_dir,
                    "registry",
                    package_name,
                    file_name,
                ))?;
            let file_delete_lock = try_acquire_registry_install_lock(
                &package_operation_lock_path(&package.cache_dir, "file", package_name, file_name),
            )?;
            if registry_delete_lock.is_none() || file_delete_lock.is_none() {
                continue;
            }
            if removed >= max_entries {
                break;
            }
            let canonical_path = fs::canonicalize(&path).map_err(|err| {
                KuError::message(format!(
                    "failed to resolve package cache '{}': {err}",
                    path.display()
                ))
            })?;
            if !canonical_path.starts_with(&canonical_packages_dir)
                || canonical_path == canonical_packages_dir
            {
                return Err(KuError::package(
                    "unsafe_cache_path",
                    "refusing to remove package data outside its cache root",
                    Span::default(),
                ));
            }
            fs::remove_dir_all(&canonical_path).map_err(|err| {
                KuError::message(format!(
                    "failed to remove cache '{}': {err}",
                    canonical_path.display()
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
    if removed < max_entries {
        removed += gc_stale_registry_staging(package, max_entries - removed, &mut scan_budget)?;
    }
    Ok(removed)
}

#[derive(Debug)]
struct RegistryGcCandidate {
    locked: LockedRegistryDependency,
    plan: RegistryDownloadPlan,
}

fn gc_locked_cache_keep_set(
    package: &PackageContext,
    scan_budget: &mut CacheGcScanBudget,
) -> KuResult<HashSet<PathBuf>> {
    let locked_files = read_locked_file_dependencies(package)?;
    let locked_registry = read_locked_registry_dependencies(package)?;
    let direct_files = package
        .manifest
        .dependencies
        .iter()
        .filter(|dependency| dependency.source.is_some())
        .map(|dependency| (dependency.name.as_str(), dependency))
        .collect::<HashMap<_, _>>();

    let mut file_targets = HashMap::<String, PathBuf>::new();
    let mut conservative_keep = HashSet::new();
    for (name, dependency) in &direct_files {
        let Some(locked) = locked_files.get(*name) else {
            continue;
        };
        if locked.requirement != dependency.version
            || dependency.source.as_deref() != Some(locked.source.as_str())
            || dependency
                .checksum
                .as_ref()
                .is_some_and(|checksum| checksum != &locked.checksum)
        {
            continue;
        }
        let expected_cache_key = file_cache_key(&locked.name, &locked.version, &locked.checksum);
        if locked.cache_key != expected_cache_key {
            return Err(KuError::package(
                "invalid_cache_key",
                format!("ku.lock has an invalid cache key for '{}'", locked.name),
                Span::default(),
            ));
        }
        let target = file_dependency_cache_root(package, &locked.name, &locked.cache_key);
        conservative_keep.insert(target.clone());
        file_targets.insert(locked.name.clone(), target);
    }

    let mut registry_candidates = HashMap::new();
    for locked in locked_registry.into_values() {
        let manifest = RegistryManifest {
            name: locked.name.clone(),
            version: locked.version.clone(),
            source: locked.url.clone(),
            checksum: locked.checksum.clone(),
            dependencies: Vec::new(),
        };
        let plan = plan_registry_download(
            &package.cache_dir,
            &manifest,
            Some(&locked.checksum),
            RegistryFetchPolicy::default(),
            Span::default(),
        )?;
        if plan.target_dir.file_name().and_then(|value| value.to_str())
            != Some(locked.cache_key.as_str())
        {
            return Err(KuError::package(
                "invalid_cache_key",
                format!("ku.lock has an invalid cache key for '{}'", manifest.name),
                Span::default(),
            ));
        }
        conservative_keep.insert(plan.target_dir.clone());
        registry_candidates.insert(locked.name.clone(), RegistryGcCandidate { locked, plan });
    }

    if package.manifest.dependencies.is_empty() {
        return Ok(HashSet::new());
    }
    if !scan_budget.is_active() {
        return Ok(conservative_keep);
    }

    let mut reachable_keep = HashSet::new();
    let mut invalid_targets = HashSet::new();
    let mut expanded = HashSet::new();
    let mut pending = package.manifest.dependencies.clone();
    let mut complete = true;
    while let Some(dependency) = pending.pop() {
        if !scan_budget.is_active() {
            return Ok(conservative_keep
                .difference(&invalid_targets)
                .cloned()
                .collect());
        }
        if let Some(file_dependency) = direct_files.get(dependency.name.as_str()) {
            let Some(locked) = locked_files.get(&dependency.name) else {
                complete = false;
                continue;
            };
            if !registry_version_satisfies_all(
                &locked.version,
                std::slice::from_ref(&dependency.version),
                Span::default(),
            )? || locked.requirement != file_dependency.version
                || file_dependency.source.as_deref() != Some(locked.source.as_str())
            {
                complete = false;
                continue;
            }
            let Some(target) = file_targets.get(&dependency.name) else {
                complete = false;
                continue;
            };
            if !expanded.insert(format!("file:{}", dependency.name)) {
                reachable_keep.insert(target.clone());
                continue;
            }
            match installed_file_cache_snapshot_with_budget(
                target,
                file_dependency,
                &locked.version,
                &locked.checksum,
                Span::default(),
                Some(scan_budget),
            ) {
                Ok(Some(_)) => {
                    if !scan_budget.is_active() {
                        return Ok(conservative_keep
                            .difference(&invalid_targets)
                            .cloned()
                            .collect());
                    }
                    let identity =
                        inspect_file_dependency_identity(target, file_dependency, Span::default())?;
                    if !scan_budget.is_active() {
                        return Ok(conservative_keep
                            .difference(&invalid_targets)
                            .cloned()
                            .collect());
                    }
                    reachable_keep.insert(target.clone());
                    pending.extend(identity.dependencies);
                }
                Ok(None) => {
                    invalid_targets.insert(target.clone());
                    complete = false;
                }
                Err(err) if err.code.as_deref() == Some("cache_gc_budget_exhausted") => {
                    return Ok(conservative_keep
                        .difference(&invalid_targets)
                        .cloned()
                        .collect());
                }
                Err(err) if err.code.as_deref() == Some("file_cache_tampered") => {
                    invalid_targets.insert(target.clone());
                    complete = false;
                }
                Err(err) => return Err(err),
            }
            continue;
        }

        let Some(candidate) = registry_candidates.get(&dependency.name) else {
            complete = false;
            continue;
        };
        if package
            .manifest
            .registry
            .as_ref()
            .map(|registry| registry.url.as_str())
            != Some(candidate.locked.registry.as_str())
            || !registry_version_satisfies_all(
                &candidate.locked.version,
                std::slice::from_ref(&dependency.version),
                Span::default(),
            )?
        {
            complete = false;
            continue;
        }
        if !expanded.insert(format!("registry:{}", dependency.name)) {
            reachable_keep.insert(candidate.plan.target_dir.clone());
            continue;
        }
        match installed_registry_cache_is_verified_with_budget(
            &candidate.plan,
            Span::default(),
            Some(scan_budget),
        ) {
            Ok(true) => {
                if !scan_budget.is_active() {
                    return Ok(conservative_keep
                        .difference(&invalid_targets)
                        .cloned()
                        .collect());
                }
                let package_root = candidate.plan.target_dir.join(REGISTRY_UNPACKED_DIR);
                let manifest = validate_package_root_identity(
                    &package_root,
                    &candidate.locked.name,
                    &candidate.locked.version,
                    Span::default(),
                )?;
                if !scan_budget.is_active() {
                    return Ok(conservative_keep
                        .difference(&invalid_targets)
                        .cloned()
                        .collect());
                }
                validate_registry_package_dependencies(&manifest.dependencies)?;
                reachable_keep.insert(candidate.plan.target_dir.clone());
                pending.extend(manifest.dependencies);
            }
            Ok(false) => {
                invalid_targets.insert(candidate.plan.target_dir.clone());
                complete = false;
            }
            Err(err) if err.code.as_deref() == Some("cache_gc_budget_exhausted") => {
                return Ok(conservative_keep
                    .difference(&invalid_targets)
                    .cloned()
                    .collect());
            }
            Err(err) if err.code.as_deref() == Some("registry_cache_tampered") => {
                invalid_targets.insert(candidate.plan.target_dir.clone());
                complete = false;
            }
            Err(err) => return Err(err),
        }
    }

    if complete {
        Ok(reachable_keep)
    } else {
        Ok(conservative_keep
            .difference(&invalid_targets)
            .cloned()
            .collect())
    }
}

fn cleanup_package_operation_lock_files(
    package: &PackageContext,
    max_files: usize,
    scan_budget: &mut CacheGcScanBudget,
) -> KuResult<()> {
    let path = package.cache_dir.join(PACKAGE_LOCKS_DIR);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to inspect package operation lock directory '{}': {err}",
                path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KuError::package(
            "unsafe_cache_path",
            "package operation lock path must be a real directory",
            Span::default(),
        ));
    }
    let mut removed = 0usize;
    for entry in fs::read_dir(&path).map_err(|err| {
        KuError::message(format!(
            "failed to read package operation lock directory '{}': {err}",
            path.display()
        ))
    })? {
        if removed >= max_files || !scan_budget.consume() {
            break;
        }
        let entry = entry.map_err(|err| {
            KuError::message(format!(
                "failed to read package operation lock entry: {err}"
            ))
        })?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path).map_err(|err| {
            KuError::message(format!(
                "failed to inspect package operation lock '{}': {err}",
                entry_path.display()
            ))
        })?;
        let file_name = entry_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != 0
            || !(file_name.starts_with("file-") || file_name.starts_with("registry-"))
            || !file_name.ends_with(".lock")
        {
            continue;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .truncate(false)
            .open(&entry_path)
            .map_err(|err| {
                KuError::message(format!(
                    "failed to open package operation lock '{}': {err}",
                    entry_path.display()
                ))
            })?;
        match file.try_lock() {
            Ok(()) => {
                file.unlock().map_err(|err| {
                    KuError::message(format!(
                        "failed to unlock package operation lock '{}': {err}",
                        entry_path.display()
                    ))
                })?;
                drop(file);
                fs::remove_file(&entry_path).map_err(|err| {
                    KuError::message(format!(
                        "failed to remove package operation lock '{}': {err}",
                        entry_path.display()
                    ))
                })?;
                removed += 1;
            }
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(err)) => {
                return Err(KuError::message(format!(
                    "failed to inspect package operation lock '{}': {err}",
                    entry_path.display()
                )));
            }
        }
    }
    if fs::read_dir(&path)
        .map_err(|err| {
            KuError::message(format!(
                "failed to read package operation lock directory '{}': {err}",
                path.display()
            ))
        })?
        .next()
        .is_none()
    {
        fs::remove_dir(&path).map_err(|err| {
            KuError::message(format!(
                "failed to remove empty package operation lock directory '{}': {err}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

struct CacheGcScanBudget {
    remaining_entries: usize,
    deadline: Instant,
}

impl CacheGcScanBudget {
    fn new(max_removals: usize) -> Self {
        let remaining_entries = max_removals
            .saturating_mul(CACHE_GC_SCAN_MULTIPLIER)
            .clamp(MIN_CACHE_GC_SCAN_ENTRIES, MAX_CACHE_GC_SCAN_ENTRIES);
        Self {
            remaining_entries,
            deadline: Instant::now() + Duration::from_secs(CACHE_GC_DEADLINE_SECS),
        }
    }

    fn for_registry_operation(deadline: Instant) -> Self {
        Self {
            // Package/archive policies cap a verification at far fewer entries.
            // This budget carries the operation deadline through the existing
            // bounded traversal without changing the GC entry limit.
            remaining_entries: MAX_CACHE_GC_SCAN_ENTRIES,
            deadline,
        }
    }

    fn consume(&mut self) -> bool {
        if !self.is_active() {
            return false;
        }
        self.remaining_entries -= 1;
        true
    }

    fn is_active(&self) -> bool {
        self.remaining_entries > 0 && Instant::now() < self.deadline
    }

    fn consume_or_error(&mut self) -> KuResult<()> {
        if self.consume() {
            Ok(())
        } else {
            Err(cache_gc_budget_exhausted_error())
        }
    }

    fn check_deadline(&self) -> KuResult<()> {
        if Instant::now() < self.deadline {
            Ok(())
        } else {
            Err(cache_gc_budget_exhausted_error())
        }
    }
}

fn cache_gc_budget_exhausted_error() -> KuError {
    KuError::package(
        "cache_gc_budget_exhausted",
        "package cache GC exhausted its bounded scan budget",
        Span::default(),
    )
}

fn cache_entry_is_stale(metadata: &fs::Metadata) -> bool {
    metadata
        .modified()
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
        .is_ok_and(|age| age >= Duration::from_secs(REGISTRY_STAGING_STALE_SECS))
}

fn gc_stale_registry_staging(
    package: &PackageContext,
    max_entries: usize,
    scan_budget: &mut CacheGcScanBudget,
) -> KuResult<usize> {
    if max_entries == 0 {
        return Ok(0);
    }
    let staging = package.cache_dir.join(".registry-downloads");
    if !staging.exists() {
        return Ok(0);
    }
    let canonical_staging = fs::canonicalize(&staging).map_err(|err| {
        KuError::message(format!(
            "failed to resolve registry staging '{}': {err}",
            staging.display()
        ))
    })?;
    let mut removed = 0;
    for entry in fs::read_dir(&staging).map_err(|err| {
        KuError::message(format!(
            "failed to read registry staging '{}': {err}",
            staging.display()
        ))
    })? {
        if removed >= max_entries || !scan_budget.consume() {
            break;
        }
        let entry = entry
            .map_err(|err| KuError::message(format!("failed to read staging entry: {err}")))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            KuError::message(format!(
                "failed to inspect registry staging '{}': {err}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        if !cache_entry_is_stale(&metadata) {
            continue;
        }
        let canonical_path = fs::canonicalize(&path).map_err(|err| {
            KuError::message(format!(
                "failed to resolve registry staging entry '{}': {err}",
                path.display()
            ))
        })?;
        if !canonical_path.starts_with(&canonical_staging) || canonical_path == canonical_staging {
            return Err(KuError::package(
                "unsafe_cache_path",
                "refusing to remove registry staging outside its cache root",
                Span::default(),
            ));
        }
        fs::remove_dir_all(&canonical_path).map_err(|err| {
            KuError::message(format!(
                "failed to remove stale registry staging '{}': {err}",
                canonical_path.display()
            ))
        })?;
        removed += 1;
    }
    Ok(removed)
}

pub(crate) fn freeze_lock_dependency(path: &Path, source: &[u8]) -> KuResult<LockDependency> {
    if source.len() as u64 > MAX_LOCKED_DEPENDENCY_BYTES {
        return Err(KuError::package(
            "lock_dependency_limit",
            format!(
                "dependency '{}' must be no larger than {MAX_LOCKED_DEPENDENCY_BYTES} bytes",
                path.display()
            ),
            Span::default(),
        ));
    }
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    stable_hash_update(&mut hash, source);
    Ok(LockDependency {
        path: path.to_path_buf(),
        cache_key: format!("ku-fnv64-{hash:016x}"),
    })
}

pub fn lock_dependencies(
    package: &PackageContext,
    dependencies: &[PathBuf],
) -> KuResult<Vec<LockDependency>> {
    if dependencies.len() > MAX_LOCK_DEPENDENCIES {
        return Err(KuError::package(
            "lock_dependency_limit",
            format!("ku.lock cannot record more than {MAX_LOCK_DEPENDENCIES} imported modules"),
            Span::default(),
        ));
    }
    let mut output = Vec::new();
    for path in dependencies {
        let metadata = fs::symlink_metadata(path).map_err(|err| {
            KuError::message(format!(
                "failed to inspect dependency '{}': {err}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_LOCKED_DEPENDENCY_BYTES
        {
            return Err(KuError::package(
                "lock_dependency_limit",
                format!(
                    "dependency '{}' must be a regular file no larger than {MAX_LOCKED_DEPENDENCY_BYTES} bytes",
                    path.display()
                ),
                Span::default(),
            ));
        }
        let mut file = fs::File::open(path).map_err(|err| {
            KuError::message(format!(
                "failed to read dependency '{}': {err}",
                path.display()
            ))
        })?;
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut bytes_read = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).map_err(|err| {
                KuError::message(format!(
                    "failed to read dependency '{}': {err}",
                    path.display()
                ))
            })?;
            if count == 0 {
                break;
            }
            bytes_read = bytes_read.saturating_add(count as u64);
            if bytes_read > metadata.len() || bytes_read > MAX_LOCKED_DEPENDENCY_BYTES {
                return Err(KuError::package(
                    "lock_dependency_limit",
                    format!(
                        "dependency '{}' changed or exceeded {MAX_LOCKED_DEPENDENCY_BYTES} bytes while ku.lock was being written",
                        path.display()
                    ),
                    Span::default(),
                ));
            }
            stable_hash_update(&mut hash, &buffer[..count]);
        }
        if bytes_read != metadata.len() {
            return Err(KuError::package(
                "lock_dependency_changed",
                format!(
                    "dependency '{}' changed while ku.lock was being written",
                    path.display()
                ),
                Span::default(),
            ));
        }
        let display_path = display_dependency_path(package, path)?;
        output.push((
            display_path,
            LockDependency {
                path: path.clone(),
                cache_key: format!("ku-fnv64-{hash:016x}"),
            },
        ));
    }
    output.sort_by(|left, right| left.0.cmp(&right.0));
    output.dedup_by(|left, right| left.0 == right.0);
    Ok(output
        .into_iter()
        .map(|(_, dependency)| dependency)
        .collect())
}

fn display_dependency_path(package: &PackageContext, path: &Path) -> KuResult<String> {
    // The cache normally lives below package_dir, so match resolved package
    // roots before the project root. Otherwise ku.lock would capture the local
    // `.ku/cache/...` layout instead of the portable `@name/path` identity.
    for dependency in &package.resolved_file_dependencies {
        let root = dependency.package_root.join(DEFAULT_IMPORT_ROOT);
        if let Some(relative) = strip_existing_path_prefix(path, &root) {
            return Ok(format!(
                "@{}/{}",
                dependency.name,
                portable_lock_path(relative, Span::default())?
            ));
        }
    }
    for dependency in &package.resolved_registry_dependencies {
        let root = dependency.package_root.join(DEFAULT_IMPORT_ROOT);
        if let Some(relative) = strip_existing_path_prefix(path, &root) {
            return Ok(format!(
                "@{}/{}",
                dependency.name,
                portable_lock_path(relative, Span::default())?
            ));
        }
    }
    if let Some(relative) = strip_existing_path_prefix(path, &package.package_dir) {
        return portable_lock_path(relative, Span::default());
    }
    Err(KuError::package(
        "non_portable_lock_path",
        format!(
            "imported module '{}' is outside the project and all resolved package roots",
            path.display()
        ),
        Span::default(),
    ))
}

fn strip_existing_path_prefix<'a>(path: &'a Path, root: &Path) -> Option<&'a Path> {
    path.strip_prefix(root).ok()
}

fn portable_lock_path(path: &Path, span: Span) -> KuResult<String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(KuError::package(
            "non_portable_lock_path",
            "package lock dependency path must be a non-empty relative path",
            span,
        ));
    }
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(KuError::package(
                "non_portable_lock_path",
                "package lock dependency path contains an unsafe component",
                span,
            ));
        };
        let value = component.to_str().ok_or_else(|| {
            KuError::package(
                "non_portable_lock_path",
                "package lock dependency path must be valid UTF-8",
                span,
            )
        })?;
        validate_portable_package_component(value, span).map_err(|_| {
            KuError::package(
                "non_portable_lock_path",
                format!("package lock dependency path component '{value}' is not portable"),
                span,
            )
        })?;
        components.push(value);
    }
    if components.is_empty() {
        return Err(KuError::package(
            "non_portable_lock_path",
            "package lock dependency path must not be empty",
            span,
        ));
    }
    let portable = components.join("/");
    if portable.len() > MAX_LOCK_DEPENDENCY_PATH_BYTES {
        return Err(KuError::package(
            "lock_dependency_limit",
            format!("package lock dependency path exceeds {MAX_LOCK_DEPENDENCY_PATH_BYTES} bytes"),
            span,
        ));
    }
    Ok(portable)
}

fn stable_hash_update(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
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
    let mut registry_url = None;
    let mut registry_public_key = None;
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
            "name" => set_manifest_once(&mut name, value, key, index + 1, span)?,
            "version" => set_manifest_once(&mut version, value, key, index + 1, span)?,
            "root" => set_manifest_once(&mut root, value, key, index + 1, span)?,
            "main" => set_manifest_once(&mut main, value, key, index + 1, span)?,
            "out" => set_manifest_once(&mut out, value, key, index + 1, span)?,
            "cache" => set_manifest_once(&mut cache, value, key, index + 1, span)?,
            "template" => set_manifest_once(&mut template, value, key, index + 1, span)?,
            "type" => set_manifest_once(&mut package_type, value, key, index + 1, span)?,
            "registry.url" => {
                set_manifest_once(&mut registry_url, value, "registry.url", index + 1, span)?
            }
            "registry.public_key" => set_manifest_once(
                &mut registry_public_key,
                value,
                "registry.public_key",
                index + 1,
                span,
            )?,
            key if key.starts_with("dep.") => {
                let dependency_name = key
                    .strip_prefix("dep.")
                    .and_then(|rest| rest.split('.').next())
                    .unwrap_or("");
                if !dependencies.contains_key(dependency_name)
                    && dependencies.len() >= MAX_REGISTRY_DEPENDENCIES
                {
                    return Err(KuError::package(
                        "dependency_limit",
                        format!(
                            "package declares more than {MAX_REGISTRY_DEPENDENCIES} dependencies"
                        ),
                        span,
                    ));
                }
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
    let registry = match (registry_url, registry_public_key) {
        (Some(url), Some(public_key)) => {
            validate_registry_base_url(&url, span)?;
            parse_ed25519_public_key(&public_key, span)?;
            Some(PackageRegistry { url, public_key })
        }
        (None, None) => None,
        _ => {
            return Err(KuError::package(
                "incomplete_registry_config",
                "ku.mod registry requires both registry.url and registry.public_key",
                span,
            ));
        }
    };
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
        registry,
        dependencies,
    })
}

fn set_manifest_once(
    target: &mut Option<String>,
    value: String,
    key: &str,
    line: usize,
    span: Span,
) -> KuResult<()> {
    if target.replace(value).is_some() {
        return Err(KuError::package(
            "duplicate_manifest_key",
            format!("duplicate ku.mod key '{key}' on line {line}"),
            span,
        ));
    }
    Ok(())
}

pub(crate) fn parse_registry_index(
    source: &str,
    index_url: &str,
    span: Span,
) -> KuResult<RegistryIndex> {
    validate_registry_url(index_url, span)?;
    let mut name = None;
    let mut versions = Vec::new();
    let mut current = None::<HashMap<String, String>>;
    for (index, raw_line) in source.lines().enumerate() {
        if raw_line.len() > MAX_REGISTRY_LINE_BYTES {
            return Err(KuError::package(
                "registry_index_limit",
                format!(
                    "registry index line {} exceeds {MAX_REGISTRY_LINE_BYTES} bytes",
                    index + 1
                ),
                span,
            ));
        }
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[version]]" {
            if let Some(fields) = current.take() {
                if versions.len() >= MAX_REGISTRY_INDEX_VERSIONS {
                    return Err(KuError::package(
                        "registry_index_limit",
                        format!("registry index exceeds {MAX_REGISTRY_INDEX_VERSIONS} versions"),
                        span,
                    ));
                }
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
        if versions.len() >= MAX_REGISTRY_INDEX_VERSIONS {
            return Err(KuError::package(
                "registry_index_limit",
                format!("registry index exceeds {MAX_REGISTRY_INDEX_VERSIONS} versions"),
                span,
            ));
        }
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

pub fn parse_package_version(version: &str, span: Span) -> KuResult<PackageVersion> {
    let mut parts = version.split('.');
    let (Some(major), Some(minor), Some(patch), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid_version_error(span));
    };
    if [major, minor, patch].iter().any(|part| {
        part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|byte| byte.is_ascii_digit())
    }) {
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

pub(crate) fn plan_registry_download(
    cache_dir: &Path,
    manifest: &RegistryManifest,
    cached_checksum: Option<&str>,
    policy: RegistryFetchPolicy,
    span: Span,
) -> KuResult<RegistryDownloadPlan> {
    validate_package_name(&manifest.name, span)?;
    parse_package_version(&manifest.version, span)?;
    validate_registry_url(&manifest.source, span)?;
    validate_registry_archive_url(&manifest.source, span)?;
    validate_sha256_checksum(&manifest.checksum, span)?;
    validate_registry_fetch_policy(policy, span)?;

    let target_dir = cache_dir
        .join(PACKAGE_CACHE_DIR)
        .join(&manifest.name)
        .join(registry_cache_key(manifest));
    let download_id = operation_nonce();
    let temporary_dir = cache_dir.join(".registry-downloads").join(format!(
        "{}-{}-{download_id}",
        manifest.name, manifest.version
    ));
    let action = if cached_checksum == Some(manifest.checksum.as_str()) {
        RegistryCacheAction::ReuseVerified
    } else {
        RegistryCacheAction::DownloadAndReplace
    };
    Ok(RegistryDownloadPlan {
        cache_dir: cache_dir.to_path_buf(),
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

#[cfg(test)]
fn execute_registry_download(plan: &RegistryDownloadPlan, span: Span) -> KuResult<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(MAX_REGISTRY_RESOLVE_SECS);
    let cache_dir = registry_plan_cache_dir(plan, span)?;
    let _usage_lease = acquire_package_usage_lease_for_cache_until(cache_dir, deadline)?;
    let http_client = RegistryHttpClient::new(plan.policy, &plan.url);
    execute_registry_download_with_http_client_until(
        plan,
        span,
        deadline,
        RegistryNetworkPolicy::Allow,
        &http_client,
    )
}

#[cfg(test)]
fn execute_registry_download_until(
    plan: &RegistryDownloadPlan,
    span: Span,
    deadline: Instant,
    network_policy: RegistryNetworkPolicy,
) -> KuResult<PathBuf> {
    let http_client = RegistryHttpClient::new(plan.policy, &plan.url);
    execute_registry_download_with_http_client_until(
        plan,
        span,
        deadline,
        network_policy,
        &http_client,
    )
}

#[cfg(test)]
fn execute_registry_download_with_http_client_until(
    plan: &RegistryDownloadPlan,
    span: Span,
    deadline: Instant,
    network_policy: RegistryNetworkPolicy,
    http_client: &RegistryHttpClient,
) -> KuResult<PathBuf> {
    Ok(execute_registry_download_with_usage_until(
        plan,
        span,
        deadline,
        network_policy,
        http_client,
    )?
    .package_root)
}

#[derive(Debug)]
struct ResolvedRegistryCache {
    package_root: PathBuf,
    usage: RegistryPackageResourceUsage,
}

fn execute_registry_download_with_usage_until(
    plan: &RegistryDownloadPlan,
    span: Span,
    deadline: Instant,
    network_policy: RegistryNetworkPolicy,
    http_client: &RegistryHttpClient,
) -> KuResult<ResolvedRegistryCache> {
    ensure_registry_deadline(deadline, span)?;
    validate_registry_fetch_policy(plan.policy, span)?;
    validate_registry_url(&plan.url, span)?;
    validate_registry_archive_url(&plan.url, span)?;
    validate_sha256_checksum(&plan.checksum, span)?;
    let cache_dir = registry_plan_cache_dir(plan, span)?;
    let package_root = plan.target_dir.join(REGISTRY_UNPACKED_DIR);
    let cache = open_registry_cache_directory(cache_dir, "registry cache root", span)?;
    if network_policy == RegistryNetworkPolicy::Allow
        && validate_existing_registry_cache_layout(plan, &cache, span)?
    {
        let mut verification_budget = CacheGcScanBudget::for_registry_operation(deadline);
        let optimistic = installed_registry_cache_verification_with_budget(
            plan,
            span,
            Some(&mut verification_budget),
        );
        ensure_registry_deadline(deadline, span)?;
        if let Ok(Some(usage)) = optimistic {
            return Ok(ResolvedRegistryCache {
                package_root,
                usage,
            });
        }
        // A failed optimistic verification is only a hint. The package OS
        // lock below serializes the authoritative re-check and any repair.
    }
    let _lock_root = ensure_registry_cache_child_directory(
        &cache,
        PACKAGE_LOCKS_DIR,
        "registry package lock root",
        span,
    )?;
    let cache_key = plan
        .target_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("package");
    let lock_path = package_operation_lock_path(cache_dir, "registry", &plan.name, cache_key);
    let _install_lock = acquire_registry_install_lock_until(&lock_path, span, deadline)?;
    let layout = if network_policy == RegistryNetworkPolicy::Deny {
        let Some(layout) = open_existing_registry_cache_layout(plan, cache, span)? else {
            return Err(offline_registry_cache_miss(plan, span, None));
        };
        layout
    } else {
        prepare_registry_cache_layout(plan, cache, span)?
    };
    let network_roots = if network_policy == RegistryNetworkPolicy::Allow {
        Some((
            ensure_registry_cache_child_directory(
                &layout.cache,
                ".registry-slots",
                "registry download slot root",
                span,
            )?,
            ensure_registry_cache_child_directory(
                &layout.cache,
                ".registry-downloads",
                "registry download staging root",
                span,
            )?,
        ))
    } else {
        None
    };
    let mut verification_budget = CacheGcScanBudget::for_registry_operation(deadline);
    let authoritative = installed_registry_cache_verification_with_budget(
        plan,
        span,
        Some(&mut verification_budget),
    );
    // A filesystem read cannot be cancelled portably once it has entered the
    // kernel, but no repair mutation may begin after the absolute deadline.
    ensure_registry_deadline(deadline, span)?;
    let quarantine_existing = match authoritative {
        Ok(Some(usage)) => {
            return Ok(ResolvedRegistryCache {
                package_root,
                usage,
            });
        }
        Ok(None) if network_policy == RegistryNetworkPolicy::Deny => {
            return Err(offline_registry_cache_miss(plan, span, None));
        }
        Err(err) if network_policy == RegistryNetworkPolicy::Deny => {
            return Err(offline_registry_cache_miss(plan, span, Some(&err)));
        }
        Err(_) => true,
        Ok(None) => false,
    };
    if quarantine_existing {
        ensure_registry_deadline(deadline, span)?;
        quarantine_corrupt_registry_cache(plan, &layout, deadline, span)?;
    }
    let Some((download_slots_root, download_staging_root)) = network_roots else {
        unreachable!("offline registry resolution returns before download setup")
    };
    let _download_slots_root = download_slots_root;
    let _download_slot = acquire_registry_download_slot(plan, span, deadline)?;
    let active_policy =
        registry_fetch_policy_for_deadline(deadline, plan.policy.max_download_bytes, span)?;
    if !registry_cache_directory_is_current(
        &download_staging_root,
        "registry download staging root",
        span,
    )? {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry download staging root changed before temporary directory creation",
            span,
        ));
    }
    match fs::symlink_metadata(&plan.temporary_dir) {
        Ok(_) => {
            return Err(KuError::package(
                "registry_temp_collision",
                format!(
                    "registry temporary directory already exists '{}'",
                    plan.temporary_dir.display()
                ),
                span,
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to inspect registry temporary directory '{}': {err}",
                plan.temporary_dir.display()
            )));
        }
    }
    fs::create_dir(&plan.temporary_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create registry temporary directory '{}': {err}",
            plan.temporary_dir.display()
        ))
    })?;
    let temporary_root =
        open_registry_cache_directory(&plan.temporary_dir, "registry temporary directory", span)?;
    if temporary_root.canonical.parent() != Some(download_staging_root.canonical.as_path())
        || !registry_cache_directory_is_current(
            &download_staging_root,
            "registry download staging root",
            span,
        )?
    {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry temporary directory escaped or its staging root changed during creation",
            span,
        ));
    }
    #[cfg(windows)]
    if !windows_path_is_strict_child(
        &download_staging_root.final_path,
        &temporary_root.final_path,
    ) {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry temporary directory final path escaped its held staging root",
            span,
        ));
    }
    let mut temporary = TemporaryDirectory::new_registry(
        plan.temporary_dir.clone(),
        download_staging_root,
        temporary_root,
    );
    temporary.validate_registry_location(span)?;
    let temporary_artifact = plan.temporary_dir.join(REGISTRY_ARTIFACT_FILE);
    download_https_to_file_with_client(
        http_client,
        &plan.url,
        &temporary_artifact,
        &plan.checksum,
        active_policy,
        deadline,
        span,
    )?;
    temporary.validate_registry_location(span)?;
    ensure_registry_deadline(deadline, span)?;
    let archive_policy = PackageArchivePolicy {
        max_compressed_bytes: active_policy.max_download_bytes,
        ..PackageArchivePolicy::default()
    };
    let unpacked = unpack_package_archive_for_identity(
        &temporary_artifact,
        &plan.temporary_dir.join(REGISTRY_UNPACKED_DIR),
        archive_policy,
        &plan.name,
        &plan.version,
        Some(deadline),
        span,
    );
    // Registry callers expose one timeout code even though the archive helper
    // also serves standalone pack/publish validation with its own error domain.
    ensure_registry_deadline(deadline, span)?;
    unpacked?;
    temporary.validate_registry_location(span)?;
    ensure_registry_deadline(deadline, span)?;
    validate_package_root_identity(
        &plan.temporary_dir.join(REGISTRY_UNPACKED_DIR),
        &plan.name,
        &plan.version,
        span,
    )?;
    let mut tree_budget = CacheGcScanBudget::for_registry_operation(deadline);
    let tree_summary = package_tree_summary_with_budget(
        &plan.temporary_dir.join(REGISTRY_UNPACKED_DIR),
        PackageArchivePolicy::default(),
        span,
        Some(&mut tree_budget),
    );
    ensure_registry_deadline(deadline, span)?;
    let tree_summary = tree_summary?;
    let mut archive_budget = CacheGcScanBudget::for_registry_operation(deadline);
    let archive_summary = package_archive_tree_summary_with_budget(
        &temporary_artifact,
        archive_policy,
        span,
        Some(&mut archive_budget),
    );
    ensure_registry_deadline(deadline, span)?;
    let archive_summary = archive_summary?;
    if tree_summary.checksum != archive_summary.checksum {
        return Err(KuError::package(
            "package_archive_integrity",
            "unpacked package tree does not match its archive contents",
            span,
        ));
    }
    temporary.validate_registry_location(span)?;
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
    // Windows refuses to move a directory while a file inside it is still open.
    drop(checksum_file);
    ensure_registry_deadline(deadline, span)?;
    temporary.validate_registry_location(span)?;
    let tree_checksum_path = plan.temporary_dir.join(REGISTRY_TREE_CHECKSUM_FILE);
    let mut tree_checksum_file = fs::File::create(&tree_checksum_path).map_err(|err| {
        KuError::message(format!(
            "failed to write registry tree checksum metadata '{}': {err}",
            tree_checksum_path.display()
        ))
    })?;
    writeln!(tree_checksum_file, "sha256-{}", archive_summary.checksum).map_err(|err| {
        KuError::message(format!(
            "failed to write registry tree checksum metadata '{}': {err}",
            tree_checksum_path.display()
        ))
    })?;
    tree_checksum_file.sync_all().map_err(|err| {
        KuError::message(format!(
            "failed to sync registry tree checksum metadata '{}': {err}",
            tree_checksum_path.display()
        ))
    })?;
    drop(tree_checksum_file);
    ensure_registry_deadline(deadline, span)?;

    temporary.validate_registry_location(span)?;
    if !registry_cache_directory_is_current(&layout.package, "registry package cache root", span)? {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry package cache root changed before immutable installation",
            span,
        ));
    }
    let compressed = fs::symlink_metadata(&temporary_artifact)
        .map_err(|err| {
            KuError::package(
                "registry_cache_tampered",
                format!("failed to inspect installed registry archive: {err}"),
                span,
            )
        })?
        .len();
    ensure_registry_deadline(deadline, span)?;
    install_immutable_registry_cache(&plan.temporary_dir, &plan.target_dir, deadline, span)?;
    temporary.keep();
    // A rename already entered into the kernel may finish after its budget.
    // Keep the fully verified immutable cache, but report the expired operation.
    ensure_registry_deadline(deadline, span)?;
    Ok(ResolvedRegistryCache {
        package_root,
        usage: RegistryPackageResourceUsage {
            compressed,
            unpacked: tree_summary.bytes,
        },
    })
}

fn offline_registry_cache_miss(
    plan: &RegistryDownloadPlan,
    span: Span,
    verification_error: Option<&KuError>,
) -> KuError {
    let detail = verification_error
        .map(|err| format!("; cached content could not be verified: {err}"))
        .unwrap_or_default();
    KuError::package(
        "offline_cache_miss",
        format!(
            "offline registry cache is missing or unverified for '{}@{}'{detail}",
            plan.name, plan.version
        ),
        span,
    )
}

fn finish_registry_index_version(
    name: Option<&str>,
    mut fields: HashMap<String, String>,
    index_url: &str,
    span: Span,
) -> KuResult<RegistryManifest> {
    let mut dependency_drafts = HashMap::<String, PackageDependencyDraft>::new();
    let mut dependency_keys = fields
        .keys()
        .filter(|key| key.starts_with("dep."))
        .cloned()
        .collect::<Vec<_>>();
    dependency_keys.sort();
    for key in dependency_keys {
        let value = fields
            .remove(&key)
            .expect("registry dependency key came from fields");
        let dependency_name = key.strip_prefix("dep.").ok_or_else(|| {
            KuError::package(
                "invalid_registry_dependency",
                "signed registry index dependencies must use dep.name = \"version\"",
                span,
            )
        })?;
        if dependency_name.contains('.') {
            return Err(KuError::package(
                "invalid_registry_dependency",
                "signed registry index dependencies only support dep.name = \"version\"",
                span,
            ));
        }
        if !dependency_drafts.contains_key(dependency_name)
            && dependency_drafts.len() >= MAX_REGISTRY_DEPENDENCIES
        {
            return Err(KuError::package(
                "dependency_limit",
                format!(
                    "registry package version declares more than {MAX_REGISTRY_DEPENDENCIES} dependencies"
                ),
                span,
            ));
        }
        parse_dependency_key(&key, value, &mut dependency_drafts, 0, span)?;
    }
    let mut dependencies = dependency_drafts
        .into_values()
        .map(|dependency| dependency.finish(span))
        .collect::<KuResult<Vec<_>>>()?;
    dependencies.sort_by(|left, right| left.name.cmp(&right.name));
    validate_registry_package_dependencies(&dependencies)?;
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
    let source = resolve_registry_url(index_url, &url, span)?;
    validate_registry_archive_url(&source, span)?;
    Ok(RegistryManifest {
        name: name.to_string(),
        version,
        source,
        checksum,
        dependencies,
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

pub fn unpack_package_archive(
    archive_path: &Path,
    output_dir: &Path,
    policy: PackageArchivePolicy,
    span: Span,
) -> KuResult<PathBuf> {
    unpack_package_archive_with_expected_root(archive_path, output_dir, policy, None, None, span)
}

fn unpack_package_archive_for_identity(
    archive_path: &Path,
    output_dir: &Path,
    policy: PackageArchivePolicy,
    package_name: &str,
    version: &str,
    deadline: Option<Instant>,
    span: Span,
) -> KuResult<PathBuf> {
    let expected_root = format!("{package_name}-{version}");
    unpack_package_archive_with_expected_root(
        archive_path,
        output_dir,
        policy,
        Some(&expected_root),
        deadline,
        span,
    )
}

pub(crate) fn validate_registry_publish_archive(
    archive_path: &Path,
    output_dir: &Path,
    package_name: &str,
    version: &str,
    deadline: Instant,
    span: Span,
) -> KuResult<Vec<PackageDependency>> {
    unpack_package_archive_for_identity(
        archive_path,
        output_dir,
        PackageArchivePolicy::default(),
        package_name,
        version,
        Some(deadline),
        span,
    )?;
    let manifest = validate_package_root_identity(output_dir, package_name, version, span)?;
    validate_registry_package_dependencies(&manifest.dependencies)?;
    Ok(manifest.dependencies)
}

fn unpack_package_archive_with_expected_root(
    archive_path: &Path,
    output_dir: &Path,
    policy: PackageArchivePolicy,
    expected_root: Option<&str>,
    deadline: Option<Instant>,
    span: Span,
) -> KuResult<PathBuf> {
    ensure_package_archive_deadline(deadline, span)?;
    validate_package_archive_policy(policy, span)?;
    let metadata = fs::metadata(archive_path).map_err(|err| {
        KuError::package(
            "invalid_package_archive",
            format!(
                "failed to read package archive '{}': {err}",
                archive_path.display()
            ),
            span,
        )
    })?;
    if metadata.len() > policy.max_compressed_bytes {
        return Err(KuError::package(
            "package_archive_limit",
            format!(
                "package archive exceeds compressed limit of {} bytes",
                policy.max_compressed_bytes
            ),
            span,
        ));
    }
    if output_dir.exists() {
        return Err(KuError::package(
            "package_archive_collision",
            format!(
                "package unpack destination already exists '{}'",
                output_dir.display()
            ),
            span,
        ));
    }
    fs::create_dir_all(output_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create package unpack destination '{}': {err}",
            output_dir.display()
        ))
    })?;

    let file = fs::File::open(archive_path).map_err(|err| {
        KuError::package(
            "invalid_package_archive",
            format!(
                "failed to open package archive '{}': {err}",
                archive_path.display()
            ),
            span,
        )
    })?;
    let mut decoder = zstd::stream::read::Decoder::new(file).map_err(|err| {
        KuError::package(
            "invalid_package_archive",
            format!(
                "package archive '{}' is not valid .tar.zst: {err}",
                archive_path.display()
            ),
            span,
        )
    })?;
    decoder
        .window_log_max(MAX_PACKAGE_ZSTD_WINDOW_LOG)
        .map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to apply package zstd window limit: {err}"),
                span,
            )
        })?;
    let mut archive = tar::Archive::new(decoder);
    let mut root_name: Option<String> = None;
    let mut paths = HashSet::new();
    let mut total_unpacked = 0u64;
    let mut entries_seen = 0usize;
    let mut saw_manifest = false;

    let entries = archive.entries().map_err(|err| {
        KuError::package(
            "invalid_package_archive",
            format!("failed to read package archive entries: {err}"),
            span,
        )
    })?;
    for entry in entries {
        ensure_package_archive_deadline(deadline, span)?;
        let mut entry = entry.map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to read package archive entry: {err}"),
                span,
            )
        })?;
        entries_seen += 1;
        if entries_seen > policy.max_files {
            return Err(KuError::package(
                "package_archive_limit",
                format!(
                    "package archive exceeds entry limit of {}",
                    policy.max_files
                ),
                span,
            ));
        }
        let entry_type = entry.header().entry_type();
        if !(entry_type.is_dir() || entry_type.is_file()) {
            return Err(KuError::package(
                "unsupported_archive_entry",
                "package archives may only contain regular files and directories",
                span,
            ));
        }
        let entry_path = entry.path().map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to read package archive path: {err}"),
                span,
            )
        })?;
        let relative = validate_archive_entry_path(&entry_path, &mut root_name, policy, span)?;
        if let Some(expected_root) = expected_root {
            let actual_root = root_name
                .as_deref()
                .expect("validated archive entries always record a root");
            if actual_root != expected_root {
                return Err(KuError::package(
                    "package_identity_mismatch",
                    format!(
                        "package archive root '{actual_root}' does not match expected root '{expected_root}'"
                    ),
                    span,
                ));
            }
        }
        if relative.as_os_str().is_empty() {
            if entry_type.is_dir() {
                continue;
            }
            return Err(KuError::package(
                "invalid_package_archive",
                "package archive root must be a directory",
                span,
            ));
        }
        validate_archive_top_level(&relative, span)?;
        let portable_path = relative.to_string_lossy().replace('\\', "/").to_lowercase();
        if !paths.insert(portable_path) {
            return Err(KuError::package(
                "duplicate_archive_path",
                format!(
                    "package archive contains a duplicate or case-conflicting path '{}'",
                    relative.display()
                ),
                span,
            ));
        }
        let target = output_dir.join(&relative);
        if entry_type.is_dir() {
            fs::create_dir_all(&target).map_err(|err| {
                KuError::message(format!(
                    "failed to create package archive directory '{}': {err}",
                    target.display()
                ))
            })?;
            continue;
        }
        let file_size = entry.header().size().map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to read package archive file size: {err}"),
                span,
            )
        })?;
        if file_size > policy.max_file_bytes {
            return Err(KuError::package(
                "package_archive_limit",
                format!(
                    "package archive file '{}' exceeds {} bytes",
                    relative.display(),
                    policy.max_file_bytes
                ),
                span,
            ));
        }
        total_unpacked = total_unpacked.saturating_add(file_size);
        if total_unpacked > policy.max_unpacked_bytes {
            return Err(KuError::package(
                "package_archive_limit",
                format!(
                    "package archive exceeds unpacked limit of {} bytes",
                    policy.max_unpacked_bytes
                ),
                span,
            ));
        }
        if relative == Path::new(MANIFEST_FILE) {
            saw_manifest = true;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                KuError::message(format!(
                    "failed to create package archive directory '{}': {err}",
                    parent.display()
                ))
            })?;
        }
        let mut output = fs::File::create(&target).map_err(|err| {
            KuError::message(format!(
                "failed to create package archive file '{}': {err}",
                target.display()
            ))
        })?;
        copy_archive_file(
            &mut entry,
            &mut output,
            policy.max_file_bytes,
            deadline,
            span,
        )?;
    }

    if root_name.is_none() {
        return Err(KuError::package(
            "invalid_package_archive",
            "package archive is empty",
            span,
        ));
    }
    if !saw_manifest {
        return Err(KuError::package(
            "missing_package_manifest",
            "package archive root must contain ku.mod",
            span,
        ));
    }
    Ok(output_dir.to_path_buf())
}

#[derive(Clone)]
struct RegistryHttpClient {
    inner: Arc<RegistryHttpClientInner>,
}

struct RegistryHttpClientInner {
    policy: RegistryFetchPolicy,
    url: String,
    agent: std::sync::OnceLock<ureq::Agent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryHttpClientPolicyAction {
    ReuseCached,
    BuildForActivePolicy,
}

fn registry_http_client_policy_action(
    cached: RegistryFetchPolicy,
    active: RegistryFetchPolicy,
) -> RegistryHttpClientPolicyAction {
    if active.connect_timeout_ms < cached.connect_timeout_ms
        || active.read_timeout_ms < cached.read_timeout_ms
    {
        RegistryHttpClientPolicyAction::BuildForActivePolicy
    } else {
        RegistryHttpClientPolicyAction::ReuseCached
    }
}

impl RegistryHttpClient {
    fn new(policy: RegistryFetchPolicy, url: &str) -> Self {
        Self {
            inner: Arc::new(RegistryHttpClientInner {
                policy,
                url: url.to_string(),
                agent: std::sync::OnceLock::new(),
            }),
        }
    }

    fn agent(&self) -> &ureq::Agent {
        self.inner
            .agent
            .get_or_init(|| registry_http_agent(self.inner.policy, &self.inner.url))
    }

    fn for_policy(&self, policy: RegistryFetchPolicy) -> Self {
        match registry_http_client_policy_action(self.inner.policy, policy) {
            RegistryHttpClientPolicyAction::ReuseCached => self.clone(),
            RegistryHttpClientPolicyAction::BuildForActivePolicy => {
                Self::new(policy, &self.inner.url)
            }
        }
    }
}

fn fetch_https_bytes_with_client(
    http_client: &RegistryHttpClient,
    url: &str,
    policy: RegistryFetchPolicy,
    deadline: Instant,
    span: Span,
) -> KuResult<Vec<u8>> {
    retry_registry_request(policy, deadline, span, |attempt_policy| {
        let http_client = http_client.for_policy(attempt_policy);
        let response = registry_get(&http_client, url, attempt_policy)?;
        let mut bytes = Vec::new();
        read_limited_until(
            response.into_reader(),
            &mut bytes,
            attempt_policy.max_download_bytes,
            Some(deadline),
            span,
        )
        .map_err(classify_stream_error)?;
        Ok(bytes)
    })
}

fn download_https_to_file_with_client(
    http_client: &RegistryHttpClient,
    url: &str,
    path: &Path,
    expected_checksum: &str,
    policy: RegistryFetchPolicy,
    deadline: Instant,
    span: Span,
) -> KuResult<()> {
    retry_registry_request(policy, deadline, span, |attempt_policy| {
        let http_client = http_client.for_policy(attempt_policy);
        let response = registry_get(&http_client, url, attempt_policy)?;
        let mut file = fs::File::create(path).map_err(|err| {
            FetchAttemptError::Fatal(KuError::message(format!(
                "failed to create registry download '{}': {err}",
                path.display()
            )))
        })?;
        let actual = stream_sha256_until(
            response.into_reader(),
            &mut file,
            attempt_policy.max_download_bytes,
            Some(deadline),
            span,
        )
        .map_err(classify_stream_error)?;
        file.sync_all().map_err(|err| {
            FetchAttemptError::Fatal(KuError::message(format!(
                "failed to sync registry download '{}': {err}",
                path.display()
            )))
        })?;
        ensure_registry_deadline(deadline, span).map_err(FetchAttemptError::Fatal)?;
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
    deadline: Instant,
    span: Span,
    mut request: impl FnMut(RegistryFetchPolicy) -> Result<T, FetchAttemptError>,
) -> KuResult<T> {
    let mut last_error = String::new();
    for attempt in 1..=policy.max_attempts {
        let remaining_attempts = policy.max_attempts - attempt + 1;
        let attempt_policy =
            registry_attempt_policy_for_deadline(policy, remaining_attempts, deadline, span)?;
        let result = request(attempt_policy);
        ensure_registry_deadline(deadline, span)?;
        match result {
            Ok(value) => return Ok(value),
            Err(FetchAttemptError::Fatal(err)) => return Err(err),
            Err(FetchAttemptError::Retry(message)) => {
                last_error = message;
                if attempt < policy.max_attempts {
                    let delay = registry_retry_delay(attempt, Instant::now(), deadline, span)?;
                    thread::sleep(delay);
                }
            }
        }
    }
    ensure_registry_deadline(deadline, span)?;
    Err(KuError::package(
        "registry_fetch_failed",
        format!(
            "registry request failed after {} attempts: {last_error}",
            policy.max_attempts
        ),
        span,
    ))
}

fn registry_retry_delay(
    attempt: u8,
    now: Instant,
    deadline: Instant,
    span: Span,
) -> KuResult<Duration> {
    let delay_ms = 25u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(7));
    let delay = Duration::from_millis(delay_ms);
    if now.checked_add(delay).is_none_or(|next| next >= deadline) {
        Err(registry_resolve_timeout_error(span))
    } else {
        Ok(delay)
    }
}

fn registry_attempt_policy_for_deadline(
    policy: RegistryFetchPolicy,
    remaining_attempts: u8,
    deadline: Instant,
    span: Span,
) -> KuResult<RegistryFetchPolicy> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| registry_resolve_timeout_error(span))?;
    let remaining_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
    let per_attempt_ms = remaining_ms / u64::from(remaining_attempts.max(1));
    if per_attempt_ms < 2 {
        return Err(registry_resolve_timeout_error(span));
    }
    let connect_timeout_ms = policy.connect_timeout_ms.min((per_attempt_ms / 3).max(1));
    let read_timeout_ms = policy
        .read_timeout_ms
        .min(per_attempt_ms.saturating_sub(connect_timeout_ms).max(1));
    Ok(RegistryFetchPolicy {
        max_attempts: remaining_attempts,
        connect_timeout_ms,
        read_timeout_ms,
        max_download_bytes: policy.max_download_bytes,
    })
}

fn registry_get(
    http_client: &RegistryHttpClient,
    url: &str,
    policy: RegistryFetchPolicy,
) -> Result<ureq::Response, FetchAttemptError> {
    match http_client
        .agent()
        .get(url)
        .timeout(Duration::from_millis(policy.read_timeout_ms))
        .set("Accept-Encoding", "identity")
        .call()
    {
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

fn registry_http_agent(policy: RegistryFetchPolicy, url: &str) -> ureq::Agent {
    #[cfg(test)]
    if let Some(agent) = test_registry_http_agent(url) {
        return agent;
    }
    #[cfg(not(test))]
    let _ = url;
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(policy.connect_timeout_ms))
        .timeout(Duration::from_millis(policy.read_timeout_ms))
        .timeout_read(Duration::from_millis(policy.read_timeout_ms))
        .redirects(0)
        .build()
}

#[cfg(test)]
fn test_registry_transport_diagnostic(transport: &ureq::Transport) -> String {
    let io = std::error::Error::source(transport)
        .and_then(|source| source.downcast_ref::<std::io::Error>());
    // Never format Transport/source: those may contain the URL, credentials or
    // peer-provided bytes. Only closed enum categories and an OS integer escape.
    format!(
        "kind={:?} io_kind={:?} os_code={:?}",
        transport.kind(),
        io.map(std::io::Error::kind),
        io.and_then(std::io::Error::raw_os_error)
    )
}

#[cfg(test)]
#[test]
fn registry_transport_test_diagnostic_excludes_source_text() {
    let error = ureq::Error::from(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "https://fixture.invalid/?private=do-not-print authorization-do-not-print",
    ));
    let ureq::Error::Transport(transport) = error else {
        panic!("expected a transport fixture")
    };
    assert_eq!(
        test_registry_transport_diagnostic(&transport),
        "kind=Io io_kind=Some(TimedOut) os_code=None"
    );
}

#[cfg(test)]
struct TestRegistryHttpAgent {
    origin: String,
    agent: ureq::Agent,
    matched_client_count: u64,
}

#[cfg(test)]
static TEST_REGISTRY_HTTP_AGENT: std::sync::OnceLock<
    std::sync::Mutex<Option<TestRegistryHttpAgent>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn test_registry_http_agent(url: &str) -> Option<ureq::Agent> {
    let mut current = TEST_REGISTRY_HTTP_AGENT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("test registry HTTP agent mutex poisoned");
    let configured = current
        .as_mut()
        .filter(|configured| url.starts_with(&configured.origin))?;
    configured.matched_client_count = configured.matched_client_count.saturating_add(1);
    Some(configured.agent.clone())
}

#[cfg(test)]
static TEST_REGISTRY_HTTP_AGENT_SESSION: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct TestRegistryHttpAgentGuard {
    _session: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl TestRegistryHttpAgentGuard {
    pub(crate) fn install(origin: String, agent: ureq::Agent) -> Self {
        let session = TEST_REGISTRY_HTTP_AGENT_SESSION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut current = TEST_REGISTRY_HTTP_AGENT
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("test registry HTTP agent mutex poisoned");
        assert!(
            current.is_none(),
            "a test registry HTTP agent is already active"
        );
        *current = Some(TestRegistryHttpAgent {
            origin,
            agent,
            matched_client_count: 0,
        });
        drop(current);
        Self { _session: session }
    }

    pub(crate) fn matched_client_count(&self) -> u64 {
        TEST_REGISTRY_HTTP_AGENT
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("test registry HTTP agent mutex poisoned")
            .as_ref()
            .expect("test registry HTTP agent guard lost its configured agent")
            .matched_client_count
    }
}

#[cfg(test)]
impl Drop for TestRegistryHttpAgentGuard {
    fn drop(&mut self) {
        *TEST_REGISTRY_HTTP_AGENT
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("test registry HTTP agent mutex poisoned") = None;
    }
}

fn read_limited(mut reader: impl Read, mut writer: impl Write, max_bytes: u64) -> KuResult<u64> {
    read_limited_until(&mut reader, &mut writer, max_bytes, None, Span::default())
}

fn read_limited_until(
    mut reader: impl Read,
    mut writer: impl Write,
    max_bytes: u64,
    deadline: Option<Instant>,
    span: Span,
) -> KuResult<u64> {
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if let Some(deadline) = deadline {
            ensure_registry_deadline(deadline, span)?;
        }
        let count = reader.read(&mut buffer).map_err(|err| {
            KuError::package(
                "registry_read_failed",
                format!("failed to read registry response: {err}"),
                Span::default(),
            )
        })?;
        if let Some(deadline) = deadline {
            ensure_registry_deadline(deadline, span)?;
        }
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
    stream_sha256_until(&mut reader, &mut writer, max_bytes, None, Span::default())
}

fn stream_sha256_until(
    mut reader: impl Read,
    mut writer: impl Write,
    max_bytes: u64,
    deadline: Option<Instant>,
    span: Span,
) -> KuResult<String> {
    let mut total = 0u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if let Some(deadline) = deadline {
            ensure_registry_deadline(deadline, span)?;
        }
        let count = reader.read(&mut buffer).map_err(|err| {
            KuError::package(
                "registry_read_failed",
                format!("failed to read registry response: {err}"),
                Span::default(),
            )
        })?;
        if let Some(deadline) = deadline {
            ensure_registry_deadline(deadline, span)?;
        }
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
    sha256_file_matches_with_budget(path, checksum, max_bytes, None)
}

fn sha256_file_matches_with_budget(
    path: &Path,
    checksum: &str,
    max_bytes: u64,
    gc_budget: Option<&mut CacheGcScanBudget>,
) -> KuResult<bool> {
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
    let mut file = file;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if let Some(budget) = gc_budget.as_deref() {
            budget.check_deadline()?;
        }
        let count = file.read(&mut buffer).map_err(|err| {
            KuError::message(format!(
                "failed to read registry cache '{}': {err}",
                path.display()
            ))
        })?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes || total > metadata.len() {
            return Ok(false);
        }
        hasher.update(&buffer[..count]);
    }
    if total != metadata.len() {
        return Ok(false);
    }
    let actual = format!("{:x}", hasher.finalize());
    Ok(checksum_hex(checksum).eq_ignore_ascii_case(&actual))
}

fn installed_registry_cache_is_verified_with_budget(
    plan: &RegistryDownloadPlan,
    span: Span,
    gc_budget: Option<&mut CacheGcScanBudget>,
) -> KuResult<bool> {
    Ok(installed_registry_cache_verification_with_budget(plan, span, gc_budget)?.is_some())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegistryPackageResourceUsage {
    compressed: u64,
    unpacked: u64,
}

fn installed_registry_cache_verification_with_budget(
    plan: &RegistryDownloadPlan,
    span: Span,
    gc_budget: Option<&mut CacheGcScanBudget>,
) -> KuResult<Option<RegistryPackageResourceUsage>> {
    let mut gc_budget = gc_budget;
    if let Some(budget) = gc_budget.as_deref_mut() {
        budget.consume_or_error()?;
    }
    let target_metadata = match fs::symlink_metadata(&plan.target_dir) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(KuError::package(
                "registry_cache_tampered",
                format!("failed to inspect registry cache directory: {err}"),
                span,
            ));
        }
    };
    if target_metadata.file_type().is_symlink() || !target_metadata.is_dir() {
        return Err(KuError::package(
            "registry_cache_tampered",
            "registry cache target must be a real directory",
            span,
        ));
    }
    let artifact = plan.target_dir.join(REGISTRY_ARTIFACT_FILE);
    let package_root = plan.target_dir.join(REGISTRY_UNPACKED_DIR);
    let package_root_metadata = fs::symlink_metadata(&package_root).ok();
    if !artifact.is_file()
        || package_root_metadata
            .as_ref()
            .is_none_or(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(KuError::package(
            "registry_cache_tampered",
            format!(
                "registry cache is incomplete for '{}@{}'; run package gc before retrying",
                plan.name, plan.version
            ),
            span,
        ));
    }
    let artifact_checksum_path = plan.target_dir.join(REGISTRY_CHECKSUM_FILE);
    let artifact_checksum_metadata =
        fs::symlink_metadata(&artifact_checksum_path).map_err(|err| {
            KuError::package(
                "registry_cache_tampered",
                format!(
                    "registry cache checksum metadata is unavailable for '{}@{}': {err}",
                    plan.name, plan.version
                ),
                span,
            )
        })?;
    if artifact_checksum_metadata.file_type().is_symlink()
        || !artifact_checksum_metadata.is_file()
        || artifact_checksum_metadata.len() > 128
    {
        return Err(KuError::package(
            "registry_cache_tampered",
            "registry cache checksum metadata is invalid",
            span,
        ));
    }
    let stored_artifact_checksum = fs::read_to_string(&artifact_checksum_path).map_err(|err| {
        KuError::package(
            "registry_cache_tampered",
            format!("failed to read registry cache checksum metadata: {err}"),
            span,
        )
    })?;
    if !stored_artifact_checksum
        .trim()
        .eq_ignore_ascii_case(&plan.checksum)
    {
        return Err(KuError::package(
            "registry_cache_tampered",
            format!(
                "registry cache checksum metadata changed for '{}@{}'",
                plan.name, plan.version
            ),
            span,
        ));
    }
    if !sha256_file_matches_with_budget(
        &artifact,
        &plan.checksum,
        plan.policy.max_download_bytes,
        gc_budget.as_deref_mut(),
    )? {
        return Err(KuError::package(
            "registry_cache_tampered",
            format!(
                "registry cache archive checksum changed for '{}@{}'",
                plan.name, plan.version
            ),
            span,
        ));
    }
    validate_package_root_identity(&package_root, &plan.name, &plan.version, span)?;
    let checksum_path = plan.target_dir.join(REGISTRY_TREE_CHECKSUM_FILE);
    let metadata = fs::symlink_metadata(&checksum_path).map_err(|err| {
        KuError::package(
            "registry_cache_tampered",
            format!(
                "registry cache integrity metadata is unavailable for '{}@{}': {err}",
                plan.name, plan.version
            ),
            span,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 128 {
        return Err(KuError::package(
            "registry_cache_tampered",
            "registry cache integrity metadata is invalid",
            span,
        ));
    }
    let expected = fs::read_to_string(&checksum_path).map_err(|err| {
        KuError::package(
            "registry_cache_tampered",
            format!("failed to read registry cache integrity metadata: {err}"),
            span,
        )
    })?;
    let expected = expected.trim();
    validate_sha256_checksum(expected, span).map_err(|_| {
        KuError::package(
            "registry_cache_tampered",
            "registry cache integrity metadata has an invalid checksum",
            span,
        )
    })?;
    let archive_tree = package_archive_tree_summary_with_budget(
        &artifact,
        PackageArchivePolicy {
            max_compressed_bytes: plan.policy.max_download_bytes,
            ..PackageArchivePolicy::default()
        },
        span,
        gc_budget.as_deref_mut(),
    )?;
    if checksum_hex(expected) != archive_tree.checksum {
        return Err(KuError::package(
            "registry_cache_tampered",
            format!(
                "registry cache integrity metadata changed for '{}@{}'",
                plan.name, plan.version
            ),
            span,
        ));
    }
    let actual = package_tree_summary_with_budget(
        &package_root,
        PackageArchivePolicy::default(),
        span,
        gc_budget,
    )?;
    if archive_tree.checksum != actual.checksum {
        return Err(KuError::package(
            "registry_cache_tampered",
            format!(
                "registry cache source tree changed for '{}@{}'",
                plan.name, plan.version
            ),
            span,
        ));
    }
    Ok(Some(RegistryPackageResourceUsage {
        compressed: fs::symlink_metadata(&artifact)
            .map_err(|err| {
                KuError::package(
                    "registry_cache_tampered",
                    format!("failed to re-inspect registry cache archive: {err}"),
                    span,
                )
            })?
            .len(),
        unpacked: actual.bytes,
    }))
}

fn package_tree_sha256(root: &Path, policy: PackageArchivePolicy, span: Span) -> KuResult<String> {
    Ok(package_tree_summary_with_budget(root, policy, span, None)?.checksum)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageTreeSummary {
    checksum: String,
    bytes: u64,
}

fn package_tree_summary_with_budget(
    root: &Path,
    policy: PackageArchivePolicy,
    span: Span,
    gc_budget: Option<&mut CacheGcScanBudget>,
) -> KuResult<PackageTreeSummary> {
    let mut records = Vec::new();
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut gc_budget = gc_budget;
    collect_package_tree_records(
        root,
        root,
        &mut records,
        &mut files,
        &mut bytes,
        policy,
        span,
        &mut gc_budget,
    )?;
    Ok(PackageTreeSummary {
        checksum: finish_package_tree_hash(records),
        bytes,
    })
}

#[derive(Debug)]
struct PackageTreeRecord {
    path: String,
    size: u64,
    digest: [u8; 32],
}

#[allow(clippy::too_many_arguments)]
fn collect_package_tree_records(
    root: &Path,
    current: &Path,
    records: &mut Vec<PackageTreeRecord>,
    files: &mut usize,
    bytes: &mut u64,
    policy: PackageArchivePolicy,
    span: Span,
    gc_budget: &mut Option<&mut CacheGcScanBudget>,
) -> KuResult<()> {
    let package_remaining = policy.max_files.saturating_sub(*files);
    let gc_remaining = gc_budget
        .as_deref()
        .map(|budget| budget.remaining_entries)
        .unwrap_or(usize::MAX);
    let remaining = package_remaining.min(gc_remaining);
    let mut entries = Vec::with_capacity(remaining.min(64));
    for entry in fs::read_dir(current).map_err(|err| {
        KuError::package(
            "registry_cache_tampered",
            format!(
                "failed to read registry cache tree '{}': {err}",
                current.display()
            ),
            span,
        )
    })? {
        if entries.len() >= remaining {
            if gc_remaining <= package_remaining {
                return Err(cache_gc_budget_exhausted_error());
            }
            return Err(KuError::package(
                "registry_cache_tampered",
                "registry cache tree exceeds package entry limit",
                span,
            ));
        }
        entries.push(entry.map_err(|err| {
            KuError::package(
                "registry_cache_tampered",
                format!("failed to read registry cache entry: {err}"),
                span,
            )
        })?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if let Some(budget) = gc_budget.as_deref_mut() {
            budget.consume_or_error()?;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|err| {
            KuError::package(
                "registry_cache_tampered",
                format!(
                    "failed to inspect registry cache '{}': {err}",
                    path.display()
                ),
                span,
            )
        })?;
        if metadata.file_type().is_symlink() || !(metadata.is_dir() || metadata.is_file()) {
            return Err(KuError::package(
                "registry_cache_tampered",
                "registry cache contains a symlink or special filesystem entry",
                span,
            ));
        }
        let relative = path.strip_prefix(root).map_err(|_| {
            KuError::package(
                "registry_cache_tampered",
                "registry cache path escaped its package root",
                span,
            )
        })?;
        let relative = relative
            .to_str()
            .ok_or_else(|| {
                KuError::package(
                    "registry_cache_tampered",
                    "registry cache paths must be valid UTF-8",
                    span,
                )
            })?
            .replace('\\', "/");
        if relative.len() > policy.max_path_bytes
            || relative.split('/').count() + 1 > policy.max_depth
        {
            return Err(KuError::package(
                "registry_cache_tampered",
                "registry cache path exceeds package limits",
                span,
            ));
        }
        for component in relative.split('/') {
            validate_portable_package_component(component, span).map_err(|_| {
                KuError::package(
                    "registry_cache_tampered",
                    "registry cache contains a non-portable path",
                    span,
                )
            })?;
        }
        *files += 1;
        if *files > policy.max_files {
            return Err(KuError::package(
                "registry_cache_tampered",
                "registry cache tree exceeds package entry limit",
                span,
            ));
        }
        if metadata.is_dir() {
            collect_package_tree_records(
                root,
                path.as_path(),
                records,
                files,
                bytes,
                policy,
                span,
                gc_budget,
            )?;
        } else {
            *bytes = bytes.saturating_add(metadata.len());
            if metadata.len() > policy.max_file_bytes || *bytes > policy.max_unpacked_bytes {
                return Err(KuError::package(
                    "registry_cache_tampered",
                    "registry cache tree exceeds package limits",
                    span,
                ));
            }
            let mut file = fs::File::open(&path).map_err(|err| {
                KuError::package(
                    "registry_cache_tampered",
                    format!(
                        "failed to open registry cache file '{}': {err}",
                        path.display()
                    ),
                    span,
                )
            })?;
            let mut content_hasher = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            let mut read_bytes = 0u64;
            loop {
                if let Some(budget) = gc_budget.as_deref() {
                    budget.check_deadline()?;
                }
                let count = file.read(&mut buffer).map_err(|err| {
                    KuError::package(
                        "registry_cache_tampered",
                        format!(
                            "failed to read registry cache file '{}': {err}",
                            path.display()
                        ),
                        span,
                    )
                })?;
                if count == 0 {
                    break;
                }
                read_bytes = read_bytes.saturating_add(count as u64);
                if read_bytes > metadata.len() || read_bytes > policy.max_file_bytes {
                    return Err(KuError::package(
                        "registry_cache_tampered",
                        "registry cache file changed while it was being verified",
                        span,
                    ));
                }
                content_hasher.update(&buffer[..count]);
            }
            if read_bytes != metadata.len() {
                return Err(KuError::package(
                    "registry_cache_tampered",
                    "registry cache file changed while it was being verified",
                    span,
                ));
            }
            records.push(PackageTreeRecord {
                path: relative,
                size: metadata.len(),
                digest: content_hasher.finalize().into(),
            });
        }
    }
    Ok(())
}

fn package_archive_tree_sha256(
    archive_path: &Path,
    policy: PackageArchivePolicy,
    span: Span,
) -> KuResult<String> {
    Ok(package_archive_tree_summary_with_budget(archive_path, policy, span, None)?.checksum)
}

fn package_archive_tree_summary_with_budget(
    archive_path: &Path,
    policy: PackageArchivePolicy,
    span: Span,
    gc_budget: Option<&mut CacheGcScanBudget>,
) -> KuResult<PackageTreeSummary> {
    validate_package_archive_policy(policy, span)?;
    let metadata = fs::metadata(archive_path).map_err(|err| {
        KuError::package(
            "invalid_package_archive",
            format!(
                "failed to inspect package archive '{}': {err}",
                archive_path.display()
            ),
            span,
        )
    })?;
    if metadata.len() > policy.max_compressed_bytes {
        return Err(KuError::package(
            "package_archive_limit",
            "package archive exceeds compressed size limit",
            span,
        ));
    }
    let file = fs::File::open(archive_path).map_err(|err| {
        KuError::package(
            "invalid_package_archive",
            format!(
                "failed to open package archive '{}': {err}",
                archive_path.display()
            ),
            span,
        )
    })?;
    let mut decoder = zstd::stream::read::Decoder::new(file).map_err(|err| {
        KuError::package(
            "invalid_package_archive",
            format!("package archive is not valid .tar.zst: {err}"),
            span,
        )
    })?;
    decoder
        .window_log_max(MAX_PACKAGE_ZSTD_WINDOW_LOG)
        .map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to apply package zstd window limit: {err}"),
                span,
            )
        })?;
    let mut archive = tar::Archive::new(decoder);
    let mut root_name = None;
    let mut paths = HashSet::new();
    let mut records = Vec::new();
    let mut entries_seen = 0usize;
    let mut bytes = 0u64;
    let mut gc_budget = gc_budget;
    for entry in archive.entries().map_err(|err| {
        KuError::package(
            "invalid_package_archive",
            format!("failed to read package archive entries: {err}"),
            span,
        )
    })? {
        if let Some(budget) = gc_budget.as_deref_mut() {
            budget.consume_or_error()?;
        }
        let mut entry = entry.map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to read package archive entry: {err}"),
                span,
            )
        })?;
        entries_seen += 1;
        if entries_seen > policy.max_files {
            return Err(KuError::package(
                "package_archive_limit",
                "package archive exceeds entry count limit",
                span,
            ));
        }
        let entry_type = entry.header().entry_type();
        if !(entry_type.is_dir() || entry_type.is_file()) {
            return Err(KuError::package(
                "unsupported_archive_entry",
                "package archives may only contain regular files and directories",
                span,
            ));
        }
        let entry_path = entry.path().map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to read package archive path: {err}"),
                span,
            )
        })?;
        let relative = validate_archive_entry_path(&entry_path, &mut root_name, policy, span)?;
        if relative.as_os_str().is_empty() {
            if entry_type.is_dir() {
                continue;
            }
            return Err(KuError::package(
                "invalid_package_archive",
                "package archive root must be a directory",
                span,
            ));
        }
        validate_archive_top_level(&relative, span)?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        if !paths.insert(relative.to_lowercase()) {
            return Err(KuError::package(
                "duplicate_archive_path",
                format!("package archive contains duplicate path '{relative}'"),
                span,
            ));
        }
        if entry_type.is_dir() {
            continue;
        }
        let size = entry.header().size().map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to read package archive file size: {err}"),
                span,
            )
        })?;
        bytes = bytes.saturating_add(size);
        if size > policy.max_file_bytes || bytes > policy.max_unpacked_bytes {
            return Err(KuError::package(
                "package_archive_limit",
                "package archive exceeds file count or size limits",
                span,
            ));
        }
        let mut hasher = Sha256::new();
        let mut read_bytes = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            if let Some(budget) = gc_budget.as_deref() {
                budget.check_deadline()?;
            }
            let count = entry.read(&mut buffer).map_err(|err| {
                KuError::package(
                    "invalid_package_archive",
                    format!("failed to read package archive file: {err}"),
                    span,
                )
            })?;
            if count == 0 {
                break;
            }
            read_bytes = read_bytes.saturating_add(count as u64);
            if read_bytes > size || read_bytes > policy.max_file_bytes {
                return Err(KuError::package(
                    "package_archive_limit",
                    "package archive file exceeds declared or configured size",
                    span,
                ));
            }
            hasher.update(&buffer[..count]);
        }
        if read_bytes != size {
            return Err(KuError::package(
                "invalid_package_archive",
                "package archive file is truncated",
                span,
            ));
        }
        records.push(PackageTreeRecord {
            path: relative,
            size,
            digest: hasher.finalize().into(),
        });
    }
    if root_name.is_none() || !records.iter().any(|record| record.path == MANIFEST_FILE) {
        return Err(KuError::package(
            "missing_package_manifest",
            "package archive root must contain ku.mod",
            span,
        ));
    }
    Ok(PackageTreeSummary {
        checksum: finish_package_tree_hash(records),
        bytes,
    })
}

fn finish_package_tree_hash(mut records: Vec<PackageTreeRecord>) -> String {
    records.sort_by(|left, right| left.path.cmp(&right.path));
    let mut hasher = Sha256::new();
    for record in records {
        hasher.update(b"file\0");
        hasher.update(record.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(record.size.to_le_bytes());
        hasher.update(record.digest);
    }
    format!("{:x}", hasher.finalize())
}

fn checksum_hex(checksum: &str) -> &str {
    checksum.strip_prefix("sha256-").unwrap_or("")
}

struct TemporaryDirectory {
    path: PathBuf,
    remove_on_drop: bool,
    registry_location: Option<RegistryTemporaryDirectoryLocation>,
}

struct RegistryTemporaryDirectoryLocation {
    parent: RegistryCacheDirectory,
    target: RegistryCacheDirectory,
}

impl TemporaryDirectory {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            remove_on_drop: true,
            registry_location: None,
        }
    }

    fn new_registry(
        path: PathBuf,
        parent: RegistryCacheDirectory,
        target: RegistryCacheDirectory,
    ) -> Self {
        Self {
            path,
            remove_on_drop: true,
            registry_location: Some(RegistryTemporaryDirectoryLocation { parent, target }),
        }
    }

    fn validate_registry_location(&self, span: Span) -> KuResult<()> {
        let Some(location) = self.registry_location.as_ref() else {
            return Ok(());
        };
        if !registry_cache_directory_is_current(
            &location.parent,
            "registry download staging root",
            span,
        )? {
            return Err(KuError::package(
                "unsafe_cache_path",
                "registry download staging root changed during installation",
                span,
            ));
        }
        let current =
            open_registry_cache_directory(&self.path, "registry temporary directory", span)?;
        if current.canonical.parent() != Some(location.parent.canonical.as_path())
            || current.identity.volume != location.target.identity.volume
            || current.identity.file != location.target.identity.file
        {
            return Err(KuError::package(
                "unsafe_cache_path",
                "registry temporary directory changed or escaped its held staging root",
                span,
            ));
        }
        #[cfg(windows)]
        if !windows_path_is_strict_child(&location.parent.final_path, &current.final_path) {
            return Err(KuError::package(
                "unsafe_cache_path",
                "registry temporary directory final path escaped its held staging root",
                span,
            ));
        }
        Ok(())
    }

    fn keep(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if self.remove_on_drop && self.validate_registry_location(Span::default()).is_ok() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct RegistryInstallLock {
    file: fs::File,
}

pub(crate) struct PackageUsageLease {
    file: fs::File,
}

impl Drop for RegistryInstallLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl Drop for PackageUsageLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) fn acquire_package_usage_lease_until(
    package: &PackageContext,
    deadline: Instant,
) -> KuResult<PackageUsageLease> {
    ensure_cache_dir(package)?;
    acquire_package_usage_lease_for_cache_until(&package.cache_dir, deadline)
}

fn acquire_package_usage_lease_for_cache_until(
    cache_dir: &Path,
    deadline: Instant,
) -> KuResult<PackageUsageLease> {
    let path = cache_dir.join(".package-usage.lock");
    let file = open_validated_package_operation_lock_file(&path)?;
    loop {
        if Instant::now() >= deadline {
            return Err(KuError::package(
                "package_cache_busy",
                "package cache remained busy for the bounded operation deadline",
                Span::default(),
            ));
        }
        match file.try_lock_shared() {
            Ok(()) => return Ok(PackageUsageLease { file }),
            Err(fs::TryLockError::WouldBlock) => {
                let delay = Duration::from_millis(REGISTRY_INSTALL_LOCK_DELAY_MS);
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(delay.min(remaining));
            }
            Err(fs::TryLockError::Error(err)) => {
                return Err(KuError::message(format!(
                    "failed to lock package usage file '{}': {err}",
                    path.display()
                )));
            }
        }
    }
}

fn acquire_registry_install_lock(path: &Path, span: Span) -> KuResult<RegistryInstallLock> {
    acquire_registry_install_lock_until(
        path,
        span,
        Instant::now()
            + Duration::from_millis(
                u64::from(REGISTRY_INSTALL_LOCK_ATTEMPTS) * REGISTRY_INSTALL_LOCK_DELAY_MS,
            ),
    )
}

fn acquire_registry_install_lock_until(
    path: &Path,
    span: Span,
    deadline: Instant,
) -> KuResult<RegistryInstallLock> {
    let file = open_validated_package_operation_lock_file(path)?;
    for attempt in 1..=REGISTRY_INSTALL_LOCK_ATTEMPTS {
        if Instant::now() >= deadline {
            return Err(registry_resolve_timeout_error(span));
        }
        match file.try_lock() {
            Ok(()) => return Ok(RegistryInstallLock { file }),
            Err(fs::TryLockError::WouldBlock) => {
                if attempt < REGISTRY_INSTALL_LOCK_ATTEMPTS {
                    let delay = Duration::from_millis(REGISTRY_INSTALL_LOCK_DELAY_MS);
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    thread::sleep(delay.min(remaining));
                }
            }
            Err(fs::TryLockError::Error(err)) => {
                return Err(KuError::message(format!(
                    "failed to lock package operation file '{}': {err}",
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

fn try_acquire_registry_install_lock(path: &Path) -> KuResult<Option<RegistryInstallLock>> {
    let file = open_validated_package_operation_lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(RegistryInstallLock { file })),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(err)) => Err(KuError::message(format!(
            "failed to inspect package operation lock '{}': {err}",
            path.display()
        ))),
    }
}

pub(crate) fn open_validated_package_operation_lock_file(path: &Path) -> KuResult<fs::File> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| {
            KuError::message(format!(
                "failed to create package operation lock directory '{}': {err}",
                parent.display()
            ))
        })?;
        let parent_metadata = fs::symlink_metadata(parent).map_err(|err| {
            KuError::message(format!(
                "failed to inspect package operation lock directory '{}': {err}",
                parent.display()
            ))
        })?;
        if parent_metadata.file_type().is_symlink()
            || archive_metadata_is_reparse_point(&parent_metadata)
            || !parent_metadata.is_dir()
        {
            return Err(KuError::package(
                "unsafe_cache_path",
                "package operation lock directory must be a real directory",
                Span::default(),
            ));
        }
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_symlink()
            || archive_metadata_is_reparse_point(&metadata)
            || !metadata.is_file()
    }) {
        return Err(KuError::package(
            "unsafe_cache_path",
            "package operation lock must be a real file",
            Span::default(),
        ));
    }
    let file = open_package_operation_lock_file(path).map_err(|err| {
        KuError::message(format!(
            "failed to inspect package operation lock '{}': {err}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(|err| {
        KuError::message(format!(
            "failed to inspect opened package operation lock '{}': {err}",
            path.display()
        ))
    })?;
    if archive_metadata_is_reparse_point(&metadata) || !metadata.is_file() {
        return Err(KuError::package(
            "unsafe_cache_path",
            "opened package operation lock must be a real file",
            Span::default(),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_package_operation_lock_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_package_operation_lock_file(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(KU_FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_package_operation_lock_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn package_operation_lock_path(cache_dir: &Path, kind: &str, name: &str, key: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(name.as_bytes());
    hasher.update(b"\0");
    hasher.update(key.as_bytes());
    cache_dir
        .join(PACKAGE_LOCKS_DIR)
        .join(format!("{kind}-{:x}.lock", hasher.finalize()))
}

fn registry_plan_cache_dir(plan: &RegistryDownloadPlan, span: Span) -> KuResult<&Path> {
    let expected = plan
        .cache_dir
        .join(PACKAGE_CACHE_DIR)
        .join(&plan.name)
        .join(registry_cache_key_for_identity(
            &plan.name,
            &plan.version,
            &plan.checksum,
        ));
    if plan.cache_dir.as_os_str().is_empty() || plan.target_dir != expected {
        return Err(KuError::package(
            "invalid_cache_path",
            "registry cache target must exactly match its content-addressed package cache path",
            span,
        ));
    }
    let temporary_parent = plan.cache_dir.join(".registry-downloads");
    let temporary_name = plan
        .temporary_dir
        .file_name()
        .and_then(|name| name.to_str());
    let temporary_prefix = format!("{}-{}-", plan.name, plan.version);
    if plan.temporary_dir.parent() != Some(temporary_parent.as_path())
        || !temporary_name
            .and_then(|name| name.strip_prefix(&temporary_prefix))
            .is_some_and(operation_nonce_is_valid)
    {
        return Err(KuError::package(
            "invalid_cache_path",
            "registry download staging must exactly match its generated cache-local path",
            span,
        ));
    }
    Ok(&plan.cache_dir)
}

struct RegistryCacheDirectory {
    path: PathBuf,
    canonical: PathBuf,
    _handle: fs::File,
    identity: ArchiveFileIdentity,
    #[cfg(windows)]
    final_path: String,
}

struct RegistryCacheLayout {
    cache: RegistryCacheDirectory,
    packages: RegistryCacheDirectory,
    package: RegistryCacheDirectory,
}

fn open_registry_cache_directory(
    path: &Path,
    description: &str,
    span: Span,
) -> KuResult<RegistryCacheDirectory> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        KuError::package(
            "unsafe_cache_path",
            format!(
                "failed to inspect {description} '{}': {err}",
                path.display()
            ),
            span,
        )
    })?;
    if metadata.file_type().is_symlink()
        || archive_metadata_is_reparse_point(&metadata)
        || !metadata.is_dir()
    {
        return Err(KuError::package(
            "unsafe_cache_path",
            format!("{description} must be a real directory"),
            span,
        ));
    }
    let handle = open_archive_source_root(path).map_err(|err| {
        KuError::package(
            "unsafe_cache_path",
            format!(
                "failed to open {description} without following links '{}': {err}",
                path.display()
            ),
            span,
        )
    })?;
    let opened_metadata = handle.metadata().map_err(|err| {
        KuError::package(
            "unsafe_cache_path",
            format!("failed to inspect opened {description}: {err}"),
            span,
        )
    })?;
    if archive_metadata_is_reparse_point(&opened_metadata) || !opened_metadata.is_dir() {
        return Err(KuError::package(
            "unsafe_cache_path",
            format!("opened {description} is not a real directory"),
            span,
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|err| {
        KuError::package(
            "unsafe_cache_path",
            format!(
                "failed to resolve {description} '{}': {err}",
                path.display()
            ),
            span,
        )
    })?;
    let identity = archive_file_identity(&handle, span, description).map_err(|err| {
        KuError::package(
            "unsafe_cache_path",
            format!("failed to fix the identity of {description}: {err}"),
            span,
        )
    })?;
    let canonical_handle = open_archive_source_root(&canonical).map_err(|err| {
        KuError::package(
            "unsafe_cache_path",
            format!("failed to reopen resolved {description}: {err}"),
            span,
        )
    })?;
    let canonical_identity =
        archive_file_identity(&canonical_handle, span, description).map_err(|err| {
            KuError::package(
                "unsafe_cache_path",
                format!("failed to verify resolved {description}: {err}"),
                span,
            )
        })?;
    if identity.volume != canonical_identity.volume || identity.file != canonical_identity.file {
        return Err(KuError::package(
            "unsafe_cache_path",
            format!("{description} changed while its path was being resolved"),
            span,
        ));
    }
    #[cfg(windows)]
    let final_path = windows_final_path(&handle).map_err(|err| {
        KuError::package(
            "unsafe_cache_path",
            format!("failed to resolve opened {description}: {err}"),
            span,
        )
    })?;
    #[cfg(windows)]
    if final_path
        != windows_final_path(&canonical_handle).map_err(|err| {
            KuError::package(
                "unsafe_cache_path",
                format!("failed to verify opened {description}: {err}"),
                span,
            )
        })?
    {
        return Err(KuError::package(
            "unsafe_cache_path",
            format!("{description} changed while its final path was being resolved"),
            span,
        ));
    }
    Ok(RegistryCacheDirectory {
        path: path.to_path_buf(),
        canonical,
        _handle: handle,
        identity,
        #[cfg(windows)]
        final_path,
    })
}

fn open_existing_registry_cache_child_directory(
    parent: &RegistryCacheDirectory,
    child_name: &str,
    description: &str,
    span: Span,
) -> KuResult<Option<RegistryCacheDirectory>> {
    if !registry_cache_directory_is_current(parent, "registry cache parent", span)? {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry cache parent changed before a child directory could be opened",
            span,
        ));
    }
    let path = parent.path.join(child_name);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to inspect {description} '{}': {err}",
                path.display()
            )));
        }
    }
    let child = open_registry_cache_directory(&path, description, span)?;
    if child.canonical.parent() != Some(parent.canonical.as_path()) {
        return Err(KuError::package(
            "unsafe_cache_path",
            format!("{description} escaped its exact parent directory"),
            span,
        ));
    }
    #[cfg(windows)]
    if !windows_path_is_strict_child(&parent.final_path, &child.final_path) {
        return Err(KuError::package(
            "unsafe_cache_path",
            format!("{description} final path escaped its exact parent directory"),
            span,
        ));
    }
    Ok(Some(child))
}

fn ensure_registry_cache_child_directory(
    parent: &RegistryCacheDirectory,
    child_name: &str,
    description: &str,
    span: Span,
) -> KuResult<RegistryCacheDirectory> {
    if let Some(child) =
        open_existing_registry_cache_child_directory(parent, child_name, description, span)?
    {
        return Ok(child);
    }
    let path = parent.path.join(child_name);
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to create {description} '{}': {err}",
                path.display()
            )));
        }
    }
    open_existing_registry_cache_child_directory(parent, child_name, description, span)?.ok_or_else(
        || {
            KuError::package(
                "unsafe_cache_path",
                format!("{description} disappeared immediately after creation"),
                span,
            )
        },
    )
}

fn open_existing_registry_cache_layout(
    plan: &RegistryDownloadPlan,
    cache: RegistryCacheDirectory,
    span: Span,
) -> KuResult<Option<RegistryCacheLayout>> {
    registry_plan_cache_dir(plan, span)?;
    let Some(packages) = open_existing_registry_cache_child_directory(
        &cache,
        PACKAGE_CACHE_DIR,
        "registry packages root",
        span,
    )?
    else {
        return Ok(None);
    };
    let Some(package) = open_existing_registry_cache_child_directory(
        &packages,
        &plan.name,
        "registry package cache root",
        span,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(RegistryCacheLayout {
        cache,
        packages,
        package,
    }))
}

fn validate_existing_registry_cache_layout(
    plan: &RegistryDownloadPlan,
    cache: &RegistryCacheDirectory,
    span: Span,
) -> KuResult<bool> {
    let current = open_registry_cache_directory(&cache.path, "registry cache root", span)?;
    Ok(open_existing_registry_cache_layout(plan, current, span)?.is_some())
}

fn prepare_registry_cache_layout(
    plan: &RegistryDownloadPlan,
    cache: RegistryCacheDirectory,
    span: Span,
) -> KuResult<RegistryCacheLayout> {
    registry_plan_cache_dir(plan, span)?;
    let packages = ensure_registry_cache_child_directory(
        &cache,
        PACKAGE_CACHE_DIR,
        "registry packages root",
        span,
    )?;
    let package = ensure_registry_cache_child_directory(
        &packages,
        &plan.name,
        "registry package cache root",
        span,
    )?;
    Ok(RegistryCacheLayout {
        cache,
        packages,
        package,
    })
}

fn registry_cache_directory_is_current(
    expected: &RegistryCacheDirectory,
    description: &str,
    span: Span,
) -> KuResult<bool> {
    let current = open_registry_cache_directory(&expected.path, description, span)?;
    Ok(current.canonical == expected.canonical
        && current.identity.volume == expected.identity.volume
        && current.identity.file == expected.identity.file
        && {
            #[cfg(windows)]
            {
                current.final_path == expected.final_path
            }
            #[cfg(not(windows))]
            {
                true
            }
        })
}

fn registry_quarantine_path(plan: &RegistryDownloadPlan) -> PathBuf {
    let cache_key = plan
        .target_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let key_digest = format!("{:x}", Sha256::digest(cache_key.as_bytes()));
    plan.target_dir.with_file_name(format!(
        "{REGISTRY_QUARANTINE_PREFIX}{key_digest}-{}",
        operation_nonce()
    ))
}

fn registry_quarantine_name_is_valid(file_name: &str) -> bool {
    let Some(value) = file_name.strip_prefix(REGISTRY_QUARANTINE_PREFIX) else {
        return false;
    };
    if value.len() < 66 {
        return false;
    }
    let (digest, nonce) = value.split_at(64);
    digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        && nonce
            .strip_prefix('-')
            .is_some_and(operation_nonce_is_valid)
}

fn ensure_registry_quarantine_capacity(
    package_directory: &Path,
    scan_budget: &mut CacheGcScanBudget,
    span: Span,
) -> KuResult<()> {
    ensure_registry_deadline(scan_budget.deadline, span)?;
    let mut entries = fs::read_dir(package_directory).map_err(|err| {
        KuError::message(format!(
            "failed to inspect registry package quarantine capacity '{}': {err}",
            package_directory.display()
        ))
    })?;
    let mut quarantines = 0usize;
    loop {
        ensure_registry_deadline(scan_budget.deadline, span)?;
        if !scan_budget.consume() {
            ensure_registry_deadline(scan_budget.deadline, span)?;
            return Err(KuError::package(
                "registry_quarantine_scan_limit",
                "registry repair could not completely inspect the package directory within its bounded scan; run 'ku package gc' for this cache and inspect retained entries before retrying",
                span,
            ));
        }
        let next = entries.next();
        ensure_registry_deadline(scan_budget.deadline, span)?;
        let Some(entry) = next else {
            return Ok(());
        };
        let entry = entry.map_err(|err| {
            KuError::message(format!(
                "failed to inspect registry package quarantine entry: {err}"
            ))
        })?;
        // Count every reserved-prefix entry, even a malformed name or a link.
        // Unknown data must not be deleted or allow repair to bypass the cap.
        if entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(REGISTRY_QUARANTINE_PREFIX.as_bytes())
        {
            quarantines += 1;
            if quarantines >= MAX_REGISTRY_QUARANTINES_PER_PACKAGE {
                return Err(KuError::package(
                    "registry_quarantine_limit",
                    format!(
                        "registry package already has {MAX_REGISTRY_QUARANTINES_PER_PACKAGE} retained quarantine entries; run 'ku package gc' for this cache and inspect entries it cannot safely remove before retrying"
                    ),
                    span,
                ));
            }
        }
    }
}

fn quarantine_corrupt_registry_cache(
    plan: &RegistryDownloadPlan,
    layout: &RegistryCacheLayout,
    deadline: Instant,
    span: Span,
) -> KuResult<PathBuf> {
    quarantine_corrupt_registry_cache_with_post_rename(plan, layout, deadline, span, |_| Ok(()))
}

fn quarantine_corrupt_registry_cache_with_post_rename(
    plan: &RegistryDownloadPlan,
    layout: &RegistryCacheLayout,
    deadline: Instant,
    span: Span,
    post_rename: impl FnOnce(&Path) -> KuResult<()>,
) -> KuResult<PathBuf> {
    ensure_registry_deadline(deadline, span)?;
    let cache_dir = registry_plan_cache_dir(plan, span)?;
    // The caller holds the content-key install lock. This additional package
    // lock makes capacity checking and rename atomic with respect to repairs
    // of every other version/content key in the same package cache.
    let _repair_lock = acquire_registry_install_lock_until(
        &package_operation_lock_path(cache_dir, "registry-repair", &plan.name, "quarantine"),
        span,
        deadline,
    )?;
    ensure_registry_deadline(deadline, span)?;
    if !registry_cache_directory_is_current(&layout.cache, "registry cache root", span)?
        || !registry_cache_directory_is_current(&layout.packages, "registry packages root", span)?
        || !registry_cache_directory_is_current(
            &layout.package,
            "registry package cache root",
            span,
        )?
    {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry cache layout changed before corrupted content could be quarantined",
            span,
        ));
    }
    let target =
        open_registry_cache_directory(&plan.target_dir, "corrupted registry cache target", span)?;
    if target.canonical.parent() != Some(layout.package.canonical.as_path()) {
        return Err(KuError::package(
            "unsafe_cache_path",
            "corrupted registry cache target escaped its exact package cache root",
            span,
        ));
    }
    #[cfg(windows)]
    if !windows_path_is_strict_child(&layout.package.final_path, &target.final_path) {
        return Err(KuError::package(
            "unsafe_cache_path",
            "corrupted registry cache final path escaped its exact package cache root",
            span,
        ));
    }
    let mut capacity_budget = CacheGcScanBudget {
        remaining_entries: MAX_REGISTRY_QUARANTINE_SCAN_ENTRIES,
        deadline,
    };
    ensure_registry_quarantine_capacity(&layout.package.path, &mut capacity_budget, span)?;
    let mut tree_budget = CacheGcScanBudget::for_registry_operation(deadline);
    let plain_tree =
        registry_quarantine_tree_is_bounded_and_plain(&plan.target_dir, &mut tree_budget);
    ensure_registry_deadline(deadline, span)?;
    if !plain_tree.map_err(|err| {
        KuError::package(
            "unsafe_cache_path",
            format!("failed to inspect corrupted registry cache before quarantine: {err}"),
            span,
        )
    })? {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry repair requires a complete, bounded plain-file tree; inspect the corrupted cache for links, special entries, or exceeded size/entry limits before retrying",
            span,
        ));
    }
    let quarantine = registry_quarantine_path(plan);
    match fs::symlink_metadata(&quarantine) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(KuError::package(
                "registry_quarantine_collision",
                "registry cache quarantine path unexpectedly already exists",
                span,
            ));
        }
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to inspect registry cache quarantine '{}': {err}",
                quarantine.display()
            )));
        }
    }
    if !registry_cache_directory_is_current(&layout.cache, "registry cache root", span)?
        || !registry_cache_directory_is_current(&layout.packages, "registry packages root", span)?
        || !registry_cache_directory_is_current(
            &layout.package,
            "registry package cache root",
            span,
        )?
        || !registry_cache_directory_is_current(&target, "corrupted registry cache target", span)?
    {
        return Err(KuError::package(
            "unsafe_cache_path",
            "registry cache layout or target changed immediately before quarantine rename",
            span,
        ));
    }
    ensure_registry_deadline(deadline, span)?;
    fs::rename(&plan.target_dir, &quarantine).map_err(|err| {
        KuError::message(format!(
            "failed to atomically quarantine corrupted registry cache '{}': {err}",
            plan.target_dir.display()
        ))
    })?;
    // Once the corrupted tree has left the trusted target, any later failure
    // stays fail-closed. Never move an unknown tree back into active cache.
    post_rename(&quarantine)?;
    let moved =
        open_registry_cache_directory(&quarantine, "quarantined registry cache target", span)?;
    if moved.canonical.parent() != Some(layout.package.canonical.as_path())
        || moved.identity.volume != target.identity.volume
        || moved.identity.file != target.identity.file
    {
        return Err(KuError::package(
            "unsafe_cache_path",
            "quarantined registry cache did not retain the expected directory identity",
            span,
        ));
    }
    ensure_registry_deadline(deadline, span)?;
    Ok(quarantine)
}

fn acquire_file_work_slot(
    cache_dir: &Path,
    span: Span,
    deadline: Instant,
) -> KuResult<RegistryInstallLock> {
    let slots_dir = cache_dir.join(".file-slots");
    fs::create_dir_all(&slots_dir).map_err(|err| {
        KuError::message(format!(
            "failed to create file dependency work slot directory '{}': {err}",
            slots_dir.display()
        ))
    })?;
    for attempt in 1..=REGISTRY_INSTALL_LOCK_ATTEMPTS {
        if Instant::now() >= deadline {
            return Err(registry_resolve_timeout_error(span));
        }
        for slot in 0..MAX_REGISTRY_PARALLELISM {
            let path = slots_dir.join(format!("slot-{slot}.lock"));
            if let Some(guard) = try_acquire_registry_install_lock(&path)? {
                return Ok(guard);
            }
        }
        if attempt < REGISTRY_INSTALL_LOCK_ATTEMPTS {
            let delay = Duration::from_millis(REGISTRY_INSTALL_LOCK_DELAY_MS);
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(delay.min(remaining));
        }
    }
    Err(KuError::package(
        "file_dependency_busy",
        format!(
            "all {MAX_REGISTRY_PARALLELISM} file dependency work slots remained busy for the bounded operation deadline"
        ),
        span,
    ))
}

fn acquire_registry_download_slot(
    plan: &RegistryDownloadPlan,
    span: Span,
    deadline: Instant,
) -> KuResult<RegistryInstallLock> {
    let cache_dir = registry_plan_cache_dir(plan, span)?;
    let cache = open_registry_cache_directory(cache_dir, "registry cache root", span)?;
    let slots = ensure_registry_cache_child_directory(
        &cache,
        ".registry-slots",
        "registry download slot root",
        span,
    )?;
    let slots_dir = &slots.path;
    for attempt in 1..=REGISTRY_INSTALL_LOCK_ATTEMPTS {
        if Instant::now() >= deadline {
            return Err(registry_resolve_timeout_error(span));
        }
        for slot in 0..MAX_REGISTRY_PARALLELISM {
            let path = slots_dir.join(format!("slot-{slot}.lock"));
            if let Some(guard) = try_acquire_registry_install_lock(&path)? {
                return Ok(guard);
            }
        }
        if attempt < REGISTRY_INSTALL_LOCK_ATTEMPTS {
            let delay = Duration::from_millis(REGISTRY_INSTALL_LOCK_DELAY_MS);
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(delay.min(remaining));
        }
    }
    Err(KuError::package(
        "registry_download_busy",
        format!(
            "all {MAX_REGISTRY_PARALLELISM} registry download slots remained busy for the bounded operation deadline"
        ),
        span,
    ))
}

fn install_immutable_registry_cache(
    source: &Path,
    target: &Path,
    deadline: Instant,
    span: Span,
) -> KuResult<()> {
    ensure_registry_deadline(deadline, span)?;
    match fs::symlink_metadata(target) {
        Ok(_) => {
            return Err(KuError::package(
                "registry_cache_conflict",
                format!(
                    "content-addressed registry cache already exists but did not verify '{}'",
                    target.display()
                ),
                span,
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(KuError::message(format!(
                "failed to inspect immutable registry cache target '{}': {err}",
                target.display()
            )));
        }
    }
    ensure_registry_deadline(deadline, span)?;
    fs::rename(source, target).map_err(|err| {
        KuError::message(format!(
            "failed to atomically install registry cache '{}': {err}",
            target.display()
        ))
    })
}

fn registry_cache_key(manifest: &RegistryManifest) -> String {
    registry_cache_key_for_identity(&manifest.name, &manifest.version, &manifest.checksum)
}

fn registry_cache_key_for_identity(name: &str, version: &str, checksum: &str) -> String {
    let digest = checksum.strip_prefix("sha256-").unwrap_or(checksum);
    format!("{name}-{version}-sha256-{digest}")
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
        if let Some(source) = &self.source {
            if source.len() > MAX_REGISTRY_URL_BYTES || file_url_path(source).is_none() {
                return Err(KuError::package(
                    "invalid_dependency_source",
                    format!(
                        "dependency '{}' local source must use an absolute canonical file:// path with forward slashes and at most {MAX_REGISTRY_URL_BYTES} bytes",
                        self.name,
                    ),
                    span,
                ));
            }
        }
        if let Some(checksum) = &self.checksum {
            validate_checksum(checksum, span)?;
            if self.source.is_none() {
                return Err(KuError::package(
                    "invalid_dependency_checksum_source",
                    format!(
                        "registry dependency '{}' gets its SHA-256 checksum from the signed index; dep.{}.checksum is only valid with file://",
                        self.name, self.name
                    ),
                    span,
                ));
            }
        }
        Ok(PackageDependency {
            name: self.name,
            version,
            source: self.source,
            checksum: self.checksum,
        })
    }
}

fn parse_dependency_key(
    key: &str,
    value: String,
    dependencies: &mut HashMap<String, PackageDependencyDraft>,
    line: usize,
    span: Span,
) -> KuResult<()> {
    let rest = key.strip_prefix("dep.").ok_or_else(|| {
        KuError::package(
            "invalid_dependency_key",
            format!("invalid dependency key on line {line}"),
            span,
        )
    })?;
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
        None => set_manifest_once(
            &mut dependency.version,
            value,
            &format!("dep.{name}"),
            line,
            span,
        )?,
        Some("source") => set_manifest_once(
            &mut dependency.source,
            value,
            &format!("dep.{name}.source"),
            line,
            span,
        )?,
        Some("checksum") => set_manifest_once(
            &mut dependency.checksum,
            value,
            &format!("dep.{name}.checksum"),
            line,
            span,
        )?,
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

pub(crate) fn validate_package_name(name: &str, span: Span) -> KuResult<()> {
    if name.len() > 64 {
        return Err(KuError::package(
            "invalid_name",
            "package name must be at most 64 bytes",
            span,
        ));
    }
    if windows_path_component_has_reserved_device_basename(name) {
        return Err(KuError::package(
            "invalid_name",
            "package name cannot use a Windows reserved device basename",
            span,
        ));
    }
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
    let total_timeout = u64::from(policy.max_attempts)
        .saturating_mul(policy.connect_timeout_ms.max(policy.read_timeout_ms));
    if total_timeout > MAX_REGISTRY_TOTAL_TIMEOUT_MS {
        return Err(KuError::package(
            "invalid_fetch_policy",
            format!(
                "registry retry timeout budget must not exceed {MAX_REGISTRY_TOTAL_TIMEOUT_MS} milliseconds"
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

fn validate_package_archive_policy(policy: PackageArchivePolicy, span: Span) -> KuResult<()> {
    if policy.max_compressed_bytes == 0
        || policy.max_unpacked_bytes == 0
        || policy.max_files == 0
        || policy.max_file_bytes == 0
        || policy.max_path_bytes == 0
        || policy.max_depth == 0
    {
        return Err(KuError::package(
            "invalid_archive_policy",
            "package archive limits must be greater than zero",
            span,
        ));
    }
    Ok(())
}

fn reject_unsafe_relative_path(kind: &str, value: &str, span: Span) -> KuResult<()> {
    if value.is_empty()
        || value.contains('\\')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.split('/').any(|component| {
            component.is_empty()
                || component == "."
                || component == ".."
                || validate_portable_package_component(component, span).is_err()
        })
    {
        return Err(KuError::package(
            "unsafe_path",
            format!("ku.mod {kind} must use a portable relative path with '/' separators"),
            span,
        ));
    }
    Ok(())
}

fn reject_unsafe_dependency_import(value: &str, span: Span) -> KuResult<()> {
    if validate_package_import_text(value, span).is_err()
        || value
            .split('/')
            .any(|component| component == "." || component == "..")
    {
        return Err(KuError::package(
            "unsafe_import",
            "package dependency import must stay inside dependency root",
            span,
        ));
    }
    Ok(())
}

pub(crate) fn validate_package_import_text(value: &str, span: Span) -> KuResult<()> {
    if value.is_empty() || value.contains('\\') || value.starts_with('/') || value.ends_with('/') {
        return Err(KuError::package(
            "non_portable_import",
            "package imports must use a non-empty relative path with '/' separators",
            span,
        ));
    }
    for component in value.split('/') {
        if component.is_empty() {
            return Err(KuError::package(
                "non_portable_import",
                "package imports must not contain empty path components",
                span,
            ));
        }
        if component == "." || component == ".." {
            continue;
        }
        validate_portable_package_component(component, span).map_err(|_| {
            KuError::package(
                "non_portable_import",
                format!("package import component '{component}' is not portable"),
                span,
            )
        })?;
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

pub(crate) fn validate_sha256_checksum(value: &str, span: Span) -> KuResult<()> {
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
    if value.len() > MAX_REGISTRY_URL_BYTES || value.chars().any(char::is_whitespace) {
        return Err(invalid_registry_url_error(span));
    }
    let parsed = Url::parse(value).map_err(|_| invalid_registry_url_error(span))?;
    validate_parsed_registry_url(&parsed, span)
}

fn validate_registry_base_url(value: &str, span: Span) -> KuResult<()> {
    validate_registry_url(value, span)?;
    let parsed = Url::parse(value).map_err(|_| invalid_registry_url_error(span))?;
    if parsed.query().is_some() || !parsed.path().ends_with('/') {
        return Err(KuError::package(
            "invalid_registry_url",
            "registry.url must omit query parameters and end with '/'",
            span,
        ));
    }
    Ok(())
}

fn parse_ed25519_public_key(value: &str, span: Span) -> KuResult<[u8; 32]> {
    let bytes = parse_prefixed_hex::<32>(
        value,
        "ed25519-",
        "invalid_registry_public_key",
        "registry.public_key must use ed25519- followed by 64 hex digits",
        span,
    )?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| {
        KuError::package(
            "invalid_registry_public_key",
            "registry.public_key is not a valid Ed25519 public key",
            span,
        )
    })?;
    Ok(bytes)
}

fn parse_ed25519_signature(value: &str, span: Span) -> KuResult<[u8; 64]> {
    parse_prefixed_hex::<64>(
        value,
        "ed25519-",
        "invalid_registry_signature",
        "registry signature must use ed25519- followed by 128 hex digits",
        span,
    )
}

fn parse_prefixed_hex<const N: usize>(
    value: &str,
    prefix: &str,
    code: &str,
    message: &str,
    span: Span,
) -> KuResult<[u8; N]> {
    let hex = value
        .strip_prefix(prefix)
        .ok_or_else(|| KuError::package(code, message, span))?;
    if hex.len() != N * 2 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(KuError::package(code, message, span));
    }
    let mut output = [0u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        let offset = index * 2;
        let hi = hex_value(hex.as_bytes()[offset])
            .ok_or_else(|| KuError::package(code, message, span))?;
        let lo = hex_value(hex.as_bytes()[offset + 1])
            .ok_or_else(|| KuError::package(code, message, span))?;
        *byte = (hi << 4) | lo;
    }
    Ok(output)
}

fn validate_registry_archive_url(value: &str, span: Span) -> KuResult<()> {
    let parsed = Url::parse(value).map_err(|_| invalid_registry_url_error(span))?;
    if parsed.query().is_some() || !parsed.path().ends_with(".tar.zst") {
        return Err(KuError::package(
            "invalid_registry_archive",
            "registry package archive must use a stable .tar.zst URL without query parameters",
            span,
        ));
    }
    Ok(())
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

fn validate_archive_entry_path(
    path: &Path,
    root_name: &mut Option<String>,
    policy: PackageArchivePolicy,
    span: Span,
) -> KuResult<PathBuf> {
    let text = path.to_str().ok_or_else(|| {
        KuError::package(
            "unsafe_archive_path",
            "package archive paths must be valid UTF-8",
            span,
        )
    })?;
    if text.contains('\\') {
        return Err(unsafe_archive_path_error(span));
    }
    if text.len() > policy.max_path_bytes {
        return Err(KuError::package(
            "unsafe_archive_path",
            format!(
                "package archive path exceeds {} bytes",
                policy.max_path_bytes
            ),
            span,
        ));
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let Some(value) = value.to_str() else {
                    return Err(KuError::package(
                        "unsafe_archive_path",
                        "package archive paths must be valid UTF-8",
                        span,
                    ));
                };
                if value.is_empty() || value == "." || value == ".." {
                    return Err(unsafe_archive_path_error(span));
                }
                validate_portable_package_component(value, span)?;
                components.push(value.to_string());
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::ParentDir
            | Component::CurDir => {
                return Err(unsafe_archive_path_error(span));
            }
        }
    }
    if components.is_empty() || components.len() > policy.max_depth {
        return Err(unsafe_archive_path_error(span));
    }
    let root = components[0].clone();
    match root_name {
        Some(existing) if existing != &root => {
            return Err(KuError::package(
                "invalid_package_archive",
                "package archive must contain exactly one root directory",
                span,
            ));
        }
        Some(_) => {}
        None => *root_name = Some(root),
    }
    let mut relative = PathBuf::new();
    for component in components.iter().skip(1) {
        relative.push(component);
    }
    Ok(relative)
}

fn validate_portable_package_component(value: &str, span: Span) -> KuResult<()> {
    let trimmed = value.trim_end_matches([' ', '.']);
    if trimmed != value
        || value.chars().any(|ch| {
            ch.is_control() || matches!(ch, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
        || windows_path_component_has_reserved_device_basename(value)
    {
        return Err(KuError::package(
            "unsafe_archive_path",
            format!("package archive path component '{value}' is not portable"),
            span,
        ));
    }
    Ok(())
}

fn windows_path_component_has_reserved_device_basename(value: &str) -> bool {
    let trimmed = value.trim_end_matches([' ', '.']);
    let base = trimmed.split('.').next().unwrap_or("");
    let upper = base.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
}

fn validate_archive_top_level(relative: &Path, span: Span) -> KuResult<()> {
    let Some(first) = relative.components().next() else {
        return Ok(());
    };
    let Component::Normal(value) = first else {
        return Err(unsafe_archive_path_error(span));
    };
    let Some(name) = value.to_str() else {
        return Err(unsafe_archive_path_error(span));
    };
    if matches!(
        name,
        MANIFEST_FILE
            | "src"
            | "docs"
            | "examples"
            | "tests"
            | "README"
            | "README.md"
            | "LICENSE"
            | "LICENSE.md"
    ) {
        Ok(())
    } else {
        Err(KuError::package(
            "unsupported_archive_path",
            format!("package archive top-level path '{name}' is not allowed"),
            span,
        ))
    }
}

fn unsafe_archive_path_error(span: Span) -> KuError {
    KuError::package(
        "unsafe_archive_path",
        "package archive paths must stay under one relative root directory",
        span,
    )
}

fn copy_archive_file(
    reader: &mut impl Read,
    writer: &mut impl Write,
    max_bytes: u64,
    deadline: Option<Instant>,
    span: Span,
) -> KuResult<u64> {
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure_package_archive_deadline(deadline, span)?;
        let count = reader.read(&mut buffer).map_err(|err| {
            KuError::package(
                "invalid_package_archive",
                format!("failed to read package archive file: {err}"),
                span,
            )
        })?;
        if count == 0 {
            return Ok(total);
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes {
            return Err(KuError::package(
                "package_archive_limit",
                format!("package archive file exceeds {max_bytes} bytes"),
                span,
            ));
        }
        writer.write_all(&buffer[..count]).map_err(|err| {
            KuError::message(format!("failed to write package archive file: {err}"))
        })?;
    }
}

fn ensure_package_archive_deadline(deadline: Option<Instant>, span: Span) -> KuResult<()> {
    if deadline.is_none_or(|deadline| Instant::now() < deadline) {
        Ok(())
    } else {
        Err(KuError::package(
            "package_archive_timeout",
            "package archive validation exceeded its request deadline",
            span,
        ))
    }
}

fn validate_package_root_identity(
    path: &Path,
    expected_name: &str,
    expected_version: &str,
    span: Span,
) -> KuResult<KuMod> {
    let manifest_path = path.join(MANIFEST_FILE);
    let source = read_manifest_file(&manifest_path)?;
    let manifest = parse_manifest(&source, span)?;
    let actual_version = manifest.version.as_deref().ok_or_else(|| {
        KuError::package(
            "missing_package_version",
            "published package ku.mod must contain version",
            span,
        )
    })?;
    if manifest.name != expected_name || actual_version != expected_version {
        return Err(KuError::package(
            "package_identity_mismatch",
            format!(
                "downloaded package identity '{}@{}' does not match signed registry metadata '{}@{}'",
                manifest.name, actual_version, expected_name, expected_version
            ),
            span,
        ));
    }
    if manifest.root.as_deref().unwrap_or(DEFAULT_IMPORT_ROOT) != DEFAULT_IMPORT_ROOT {
        return Err(KuError::package(
            "unsupported_package_root",
            format!(
                "published package '{}@{}' must use the single import root '{}'",
                manifest.name, actual_version, DEFAULT_IMPORT_ROOT
            ),
            span,
        ));
    }
    let import_root = path.join(DEFAULT_IMPORT_ROOT);
    let metadata = fs::symlink_metadata(&import_root).map_err(|err| {
        KuError::package(
            "missing_package_root",
            format!(
                "published package import root '{}' is unavailable: {err}",
                import_root.display()
            ),
            span,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KuError::package(
            "invalid_package_root",
            "published package import root must be a real directory",
            span,
        ));
    }
    Ok(manifest)
}

fn read_manifest_file(path: &Path) -> KuResult<String> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        KuError::package(
            "invalid_package_manifest",
            format!(
                "failed to inspect package manifest '{}': {err}",
                path.display()
            ),
            Span::default(),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(KuError::package(
            "invalid_package_manifest",
            "ku.mod must be a regular file, not a symlink or special entry",
            Span::default(),
        ));
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(KuError::package(
            "manifest_limit",
            format!("ku.mod exceeds {MAX_MANIFEST_BYTES} bytes"),
            Span::default(),
        ));
    }
    fs::read_to_string(path).map_err(|err| {
        KuError::package(
            "invalid_package_manifest",
            format!(
                "failed to read package manifest '{}': {err}",
                path.display()
            ),
            Span::default(),
        )
    })
}

fn file_dependency_cache_root(package: &PackageContext, name: &str, cache_key: &str) -> PathBuf {
    package
        .cache_dir
        .join(PACKAGE_CACHE_DIR)
        .join(name)
        .join(cache_key)
}

fn file_cache_key(name: &str, version: &str, checksum: &str) -> String {
    let digest = checksum.strip_prefix("ku-fnv64-").unwrap_or(checksum);
    format!("{name}-{version}-fnv64-{digest}")
}

fn file_url_path(source: &str) -> Option<PathBuf> {
    let raw = source.strip_prefix("file://")?;
    if raw.is_empty() || raw.starts_with("//") || raw.contains('\\') {
        return None;
    }
    let path = if raw.starts_with('/') && raw.as_bytes().get(2) == Some(&b':') {
        &raw[1..]
    } else {
        raw
    };
    let decoded = percent_decode(path)?;
    if decoded.contains('\\') || decoded.chars().any(char::is_control) {
        return None;
    }
    let path = PathBuf::from(decoded);
    path.is_absolute().then_some(path)
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
    let mut entries = 0usize;
    let mut bytes = 0u64;
    copy_package_source_inner(source, source, target, &mut entries, &mut bytes, 0)
}

fn copy_package_source_inner(
    root: &Path,
    current: &Path,
    target_root: &Path,
    entries: &mut usize,
    bytes: &mut u64,
    depth: usize,
) -> KuResult<()> {
    if depth > MAX_PACKAGE_DEPTH {
        return Err(KuError::package(
            "cache_limit",
            "package source exceeds directory depth limit",
            Span::default(),
        ));
    }
    for entry in fs::read_dir(current).map_err(|err| {
        KuError::message(format!(
            "failed to read package source '{}': {err}",
            current.display()
        ))
    })? {
        let entry = entry
            .map_err(|err| KuError::message(format!("failed to read package entry: {err}")))?;
        let path = entry.path();
        *entries += 1;
        if *entries > MAX_PACKAGE_FILES {
            return Err(KuError::package(
                "cache_limit",
                "package source exceeds entry count limit",
                Span::default(),
            ));
        }
        if !file_snapshot_entry_is_included(root, current, &path)? {
            continue;
        }
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
            if relative == Path::new(DEFAULT_IMPORT_ROOT) {
                fs::create_dir_all(&target).map_err(|err| {
                    KuError::message(format!(
                        "failed to create package cache directory '{}': {err}",
                        target.display()
                    ))
                })?;
            }
            copy_package_source_inner(root, &path, target_root, entries, bytes, depth + 1)?;
        } else if metadata.is_file() {
            *bytes += metadata.len();
            if *bytes > MAX_PACKAGE_BYTES {
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
            let source_file = fs::File::open(&path).map_err(|err| {
                KuError::message(format!(
                    "failed to open package file '{}': {err}",
                    path.display()
                ))
            })?;
            let mut target_file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|err| {
                    KuError::message(format!(
                        "failed to create package cache file '{}': {err}",
                        target.display()
                    ))
                })?;
            let copied = io::copy(
                &mut source_file.take(metadata.len().saturating_add(1)),
                &mut target_file,
            )
            .map_err(|err| {
                KuError::message(format!(
                    "failed to copy package file '{}' to '{}': {err}",
                    path.display(),
                    target.display()
                ))
            })?;
            if copied != metadata.len() {
                return Err(KuError::package(
                    "source_changed",
                    format!(
                        "package source file '{}' changed while it was copied",
                        path.display()
                    ),
                    Span::default(),
                ));
            }
            target_file.sync_all().map_err(|err| {
                KuError::message(format!(
                    "failed to sync package cache file '{}': {err}",
                    target.display()
                ))
            })?;
        } else {
            return Err(KuError::package(
                "unsupported_package_entry",
                format!(
                    "package source contains unsupported special entry '{}'",
                    path.display()
                ),
                Span::default(),
            ));
        }
    }
    Ok(())
}

fn collect_source_hash(
    root: &Path,
    current: &Path,
    hash: &mut u64,
    entries_seen: &mut usize,
    bytes: &mut u64,
    depth: usize,
    gc_budget: &mut Option<&mut CacheGcScanBudget>,
) -> KuResult<()> {
    if depth > MAX_PACKAGE_DEPTH {
        return Err(KuError::package(
            "checksum_limit",
            "package source exceeds directory depth limit",
            Span::default(),
        ));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(current).map_err(|err| {
        KuError::message(format!(
            "failed to read package source '{}': {err}",
            current.display()
        ))
    })? {
        let entry = entry.map_err(|err| {
            KuError::message(format!("failed to read package source entry: {err}"))
        })?;
        if let Some(budget) = gc_budget.as_deref_mut() {
            budget.consume_or_error()?;
        }
        *entries_seen += 1;
        if *entries_seen > MAX_PACKAGE_FILES {
            return Err(KuError::package(
                "checksum_limit",
                "package source exceeds entry count limit",
                Span::default(),
            ));
        }
        if !file_snapshot_entry_is_included(root, current, &entry.path())? {
            continue;
        }
        entries.push(entry);
    }
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
            collect_source_hash(root, &path, hash, entries_seen, bytes, depth + 1, gc_budget)?;
        } else if metadata.is_file() {
            *bytes += metadata.len();
            if *bytes > MAX_PACKAGE_BYTES {
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
            stable_hash_update(
                hash,
                relative.to_string_lossy().replace('\\', "/").as_bytes(),
            );
            stable_hash_update(hash, b"\0");
            let mut file = fs::File::open(&path).map_err(|err| {
                KuError::message(format!(
                    "failed to read package file '{}': {err}",
                    path.display()
                ))
            })?;
            let mut read_bytes = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                if let Some(budget) = gc_budget.as_deref() {
                    budget.check_deadline()?;
                }
                let count = file.read(&mut buffer).map_err(|err| {
                    KuError::message(format!(
                        "failed to read package file '{}': {err}",
                        path.display()
                    ))
                })?;
                if count == 0 {
                    break;
                }
                read_bytes = read_bytes.saturating_add(count as u64);
                if read_bytes > metadata.len() {
                    return Err(KuError::package(
                        "source_changed",
                        format!(
                            "package source file '{}' changed while it was checksummed",
                            path.display()
                        ),
                        Span::default(),
                    ));
                }
                stable_hash_update(hash, &buffer[..count]);
            }
            if read_bytes != metadata.len() {
                return Err(KuError::package(
                    "source_changed",
                    format!(
                        "package source file '{}' changed while it was checksummed",
                        path.display()
                    ),
                    Span::default(),
                ));
            }
            stable_hash_update(hash, b"\0");
        } else {
            return Err(KuError::package(
                "unsupported_package_entry",
                format!(
                    "package source contains unsupported special entry '{}'",
                    path.display()
                ),
                Span::default(),
            ));
        }
    }
    Ok(())
}

fn file_snapshot_entry_is_included(root: &Path, current: &Path, path: &Path) -> KuResult<bool> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            KuError::package(
                "non_portable_package_path",
                "file dependency source paths must be valid UTF-8",
                Span::default(),
            )
        })?;
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        ".git" | ".hg" | ".svn" | ".ku" | "target" | "node_modules" | LOCK_FILE | ".env"
    ) || lower.starts_with(".env.")
    {
        return Ok(false);
    }
    if current != root {
        return Ok(true);
    }
    Ok(name == MANIFEST_FILE || name == DEFAULT_IMPORT_ROOT)
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::{
        io::Cursor,
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
            Arc, Barrier, Mutex,
        },
    };

    fn temp_path(label: &str) -> PathBuf {
        let id = NEXT_REGISTRY_DOWNLOAD_ID.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!("ku-{label}-{}-{id}", std::process::id()))
    }

    struct TestEnvironmentVariableGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl TestEnvironmentVariableGuard {
        fn unset(key: &'static str) -> Self {
            let previous = env::var_os(key);
            env::remove_var(key);
            Self { key, previous }
        }

        fn set(&self, value: &str) {
            env::set_var(self.key, value);
        }
    }

    impl Drop for TestEnvironmentVariableGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.take() {
                env::set_var(self.key, previous);
            } else {
                env::remove_var(self.key);
            }
        }
    }

    fn archive_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, *path, Cursor::new(*data))
                    .expect("append archive file");
            }
            builder.finish().expect("finish tar archive");
        }
        zstd::stream::encode_all(Cursor::new(tar_bytes), 0).expect("encode zstd archive")
    }

    fn archive_with_special_entry(path: &str, target: &str, entry_type: tar::EntryType) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(entry_type);
            header.set_size(0);
            header.set_mode(0o644);
            header.set_link_name(target).expect("set link target");
            header.set_cksum();
            builder
                .append_data(&mut header, path, io::empty())
                .expect("append special archive entry");
            builder.finish().expect("finish tar archive");
        }
        zstd::stream::encode_all(Cursor::new(tar_bytes), 0).expect("encode zstd archive")
    }

    fn archive_with_directories(directory_count: usize) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut root = tar::Header::new_gnu();
            root.set_entry_type(tar::EntryType::Directory);
            root.set_size(0);
            root.set_mode(0o755);
            root.set_cksum();
            builder
                .append_data(&mut root, "math/", io::empty())
                .expect("append archive root");
            let manifest = b"name = \"math\"\nversion = \"1.0.0\"\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "math/ku.mod", Cursor::new(manifest))
                .expect("append manifest");
            for index in 0..directory_count {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("math/docs/d{index}/"), io::empty())
                    .expect("append empty directory");
            }
            builder.finish().expect("finish tar archive");
        }
        zstd::stream::encode_all(Cursor::new(tar_bytes), 0).expect("encode zstd archive")
    }

    fn write_archive(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("write archive");
    }

    fn sha256_checksum(bytes: &[u8]) -> String {
        format!("sha256-{:x}", Sha256::digest(bytes))
    }

    fn ed25519_signature_text(signing_key: &SigningKey, bytes: &[u8]) -> Vec<u8> {
        let signature = signing_key.sign(bytes).to_bytes();
        let mut output = String::with_capacity("ed25519-".len() + signature.len() * 2 + 1);
        output.push_str("ed25519-");
        for byte in signature {
            output.push_str(&format!("{byte:02x}"));
        }
        output.push('\n');
        output.into_bytes()
    }

    #[test]
    fn pinned_ed25519_verifier_rejects_tampered_index_bytes() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let index_url = "https://registry.example/v1/packages/math/index.toml";
        let index = b"name = \"math\"\n\n[[version]]\nversion = \"1.0.0\"\n";
        let verifier = Ed25519RegistryIndexVerifier::new(
            signing_key.verifying_key().to_bytes(),
            signing_key.sign(index).to_bytes(),
        );

        verifier
            .verify(index_url, index, Span::default())
            .expect("the pinned key must accept the signed index bytes");
        let err = verifier
            .verify(
                index_url,
                b"name = \"math\"\n\n[[version]]\nversion = \"1.0.1\"\n",
                Span::default(),
            )
            .expect_err("tampered index bytes must fail closed");
        assert_eq!(err.code.as_deref(), Some("registry_signature_mismatch"));
    }

    #[test]
    fn publish_response_drain_is_bounded() {
        let response = ureq::Response::new(201, "Created", "published")
            .expect("create small publish response");
        drain_publish_response(response).expect("small publish response must be fully drained");

        let oversized = "x".repeat(MAX_REGISTRY_PUBLISH_RESPONSE_BYTES as usize + 1);
        let response = ureq::Response::new(201, "Created", &oversized)
            .expect("create oversized publish response");
        let err = drain_publish_response(response)
            .expect_err("oversized publish response must stop at the hard limit");
        assert_eq!(err.code.as_deref(), Some("registry_publish_response_limit"));
    }

    #[derive(Debug)]
    struct TestHttpRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    fn read_test_http_request(reader: &mut impl Read) -> io::Result<TestHttpRequest> {
        const MAX_TEST_HEADER_BYTES: usize = 64 * 1024;
        let mut bytes = Vec::new();
        let header_end = loop {
            if bytes.len() >= MAX_TEST_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "test HTTP request headers exceed limit",
                ));
            }
            let mut buffer = [0u8; 4096];
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "test HTTP request ended before headers",
                ));
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset;
            }
        };
        let header_text = std::str::from_utf8(&bytes[..header_end]).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "test HTTP request headers are not UTF-8",
            )
        })?;
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "test HTTP request line missing")
        })?;
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap_or_default().to_string();
        let path = request_parts.next().unwrap_or_default().to_string();
        if method.is_empty() || path.is_empty() || request_parts.next() != Some("HTTP/1.1") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid test HTTP request line",
            ));
        }
        let mut headers = HashMap::new();
        for line in lines {
            let (name, value) = line.split_once(':').ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid test HTTP header")
            })?;
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
        let content_length = headers
            .get("content-length")
            .map(|value| {
                value.parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid test content length")
                })
            })
            .transpose()?
            .unwrap_or(0);
        if content_length > MAX_REGISTRY_DOWNLOAD_BYTES as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "test HTTP request body exceeds registry limit",
            ));
        }
        let mut body = bytes[header_end + 4..].to_vec();
        if body.len() > content_length {
            body.truncate(content_length);
        }
        while body.len() < content_length {
            let remaining = content_length - body.len();
            let mut buffer = [0u8; 64 * 1024];
            let read_len = remaining.min(buffer.len());
            let count = reader.read(&mut buffer[..read_len])?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "test HTTP request body ended early",
                ));
            }
            body.extend_from_slice(&buffer[..count]);
        }
        Ok(TestHttpRequest {
            method,
            path,
            headers,
            body,
        })
    }

    fn write_test_http_response(
        writer: &mut impl Write,
        status: u16,
        body: &[u8],
    ) -> io::Result<()> {
        let reason = match status {
            200 => "OK",
            201 => "Created",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            _ => "Internal Server Error",
        };
        write!(
            writer,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        writer.write_all(body)?;
        writer.flush()
    }

    #[derive(Debug, Default)]
    struct TestHttpsRegistryState {
        artifact: Option<Vec<u8>>,
        current_index: Option<Vec<u8>>,
        current_signature: Option<Vec<u8>>,
        signature_reads: usize,
        index_reads: usize,
        artifact_reads: usize,
        successful_uploads: usize,
        rejected_uploads: usize,
        protocol_errors: Vec<String>,
    }

    struct TestHttpsRegistry {
        base_url: String,
        state: Arc<Mutex<TestHttpsRegistryState>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TestHttpsRegistry {
        fn start(signing_key: SigningKey, publish_token: &'static str) -> (Self, ureq::Agent) {
            Self::start_for(signing_key, publish_token, "greeter", "1.0.0", Vec::new())
        }

        fn start_for(
            signing_key: SigningKey,
            publish_token: &'static str,
            package_name: &'static str,
            package_version: &'static str,
            package_dependencies: Vec<PackageDependency>,
        ) -> (Self, ureq::Agent) {
            use rcgen::{generate_simple_self_signed, CertifiedKey};
            use ureq::rustls::{
                pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
                ClientConfig, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
            };

            let CertifiedKey { cert, key_pair } =
                generate_simple_self_signed(vec!["localhost".to_string()])
                    .expect("generate test TLS certificate");
            let certificate = cert.der().clone();
            let private_key =
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
            let server_config = Arc::new(
                ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(vec![certificate.clone()], private_key)
                    .expect("configure test TLS server"),
            );
            let mut roots = RootCertStore::empty();
            roots.add(certificate).expect("trust test TLS certificate");
            let client_config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let agent = ureq::AgentBuilder::new()
                .tls_config(Arc::new(client_config))
                .timeout_connect(Duration::from_secs(2))
                .timeout(Duration::from_secs(3))
                .timeout_read(Duration::from_secs(3))
                .redirects(0)
                .build();

            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test TLS registry");
            listener
                .set_nonblocking(true)
                .expect("set test TLS registry nonblocking");
            let port = listener.local_addr().expect("test registry address").port();
            let base_url = format!("https://localhost:{port}/v1/");
            let state = Arc::new(Mutex::new(TestHttpsRegistryState::default()));
            let thread_state = Arc::clone(&state);
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                while !thread_stop.load(AtomicOrdering::Acquire) {
                    let (stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(err) => {
                            thread_state
                                .lock()
                                .expect("test registry state mutex poisoned")
                                .protocol_errors
                                .push(format!("accept failed: {err}"));
                            return;
                        }
                    };
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    let connection = match ServerConnection::new(Arc::clone(&server_config)) {
                        Ok(connection) => connection,
                        Err(err) => {
                            thread_state
                                .lock()
                                .expect("test registry state mutex poisoned")
                                .protocol_errors
                                .push(format!("TLS connection setup failed: {err}"));
                            continue;
                        }
                    };
                    let mut tls = StreamOwned::new(connection, stream);
                    let request = match read_test_http_request(&mut tls) {
                        Ok(request) => request,
                        Err(err) => {
                            thread_state
                                .lock()
                                .expect("test registry state mutex poisoned")
                                .protocol_errors
                                .push(format!("request read failed: {err}"));
                            continue;
                        }
                    };
                    let (status, body) = handle_test_registry_request(
                        request,
                        &thread_state,
                        &signing_key,
                        publish_token,
                        package_name,
                        package_version,
                        &package_dependencies,
                    );
                    if let Err(err) = write_test_http_response(&mut tls, status, &body) {
                        thread_state
                            .lock()
                            .expect("test registry state mutex poisoned")
                            .protocol_errors
                            .push(format!("response write failed: {err}"));
                    }
                }
            });
            (
                Self {
                    base_url,
                    state,
                    stop,
                    thread: Some(handle),
                },
                agent,
            )
        }
    }

    impl Drop for TestHttpsRegistry {
        fn drop(&mut self) {
            self.stop.store(true, AtomicOrdering::Release);
            if let Some(handle) = self.thread.take() {
                handle.join().expect("test TLS registry thread panicked");
            }
        }
    }

    fn handle_test_registry_request(
        request: TestHttpRequest,
        state: &Arc<Mutex<TestHttpsRegistryState>>,
        signing_key: &SigningKey,
        publish_token: &str,
        package_name: &str,
        package_version: &str,
        package_dependencies: &[PackageDependency],
    ) -> (u16, Vec<u8>) {
        let upload_path = format!("/v1/packages/{package_name}/{package_version}");
        let index_path = format!("/v1/packages/{package_name}/index.toml");
        let signature_path = format!("{index_path}.sig");
        let mut current = state.lock().expect("test registry state mutex poisoned");
        let artifact_path = current.artifact.as_ref().map(|artifact| {
            format!(
                "/v1/artifacts/{package_name}-{package_version}-{}.tar.zst",
                sha256_checksum(artifact)
            )
        });
        match request.method.as_str() {
            "PUT" if request.path == upload_path => {
                if request.headers.get("authorization").map(String::as_str)
                    != Some(&format!("Bearer {publish_token}"))
                {
                    current.rejected_uploads += 1;
                    return (403, Vec::new());
                }
                let actual_checksum = sha256_checksum(&request.body);
                let expected_idempotency =
                    format!("{package_name}-{package_version}-{actual_checksum}");
                let valid_headers = request.headers.get("content-type").map(String::as_str)
                    == Some(PACKAGE_CONTENT_TYPE)
                    && request.headers.get("x-ku-checksum") == Some(&actual_checksum)
                    && request.headers.get("idempotency-key") == Some(&expected_idempotency);
                if !valid_headers {
                    current
                        .protocol_errors
                        .push("publish headers did not match the v1 contract".to_string());
                    return (400, Vec::new());
                }
                if let Some(existing) = &current.artifact {
                    return if existing == &request.body {
                        (200, Vec::new())
                    } else {
                        (409, Vec::new())
                    };
                }
                let mut index = format!(
                    "name = \"{package_name}\"\n\n[[version]]\nversion = \"{package_version}\"\nurl = \"../../artifacts/{package_name}-{package_version}-{actual_checksum}.tar.zst\"\nchecksum = \"{actual_checksum}\"\n"
                );
                for dependency in package_dependencies {
                    index.push_str(&format!(
                        "dep.{} = {:?}\n",
                        dependency.name, dependency.version
                    ));
                }
                let index = index.into_bytes();
                let signature = ed25519_signature_text(signing_key, &index);
                current.artifact = Some(request.body);
                current.current_index = Some(index);
                current.current_signature = Some(signature);
                current.successful_uploads += 1;
                (201, Vec::new())
            }
            "GET" if request.path == signature_path => {
                current.signature_reads += 1;
                let body = if current.signature_reads == 1 {
                    ed25519_signature_text(signing_key, b"name = \"stale-generation\"\n")
                } else {
                    current.current_signature.clone().unwrap_or_default()
                };
                (if body.is_empty() { 404 } else { 200 }, body)
            }
            "GET" if request.path == index_path => {
                current.index_reads += 1;
                let body = current.current_index.clone().unwrap_or_default();
                (if body.is_empty() { 404 } else { 200 }, body)
            }
            "GET" if artifact_path.as_deref() == Some(request.path.as_str()) => {
                current.artifact_reads += 1;
                let body = current.artifact.clone().unwrap_or_default();
                (if body.is_empty() { 404 } else { 200 }, body)
            }
            _ => (404, Vec::new()),
        }
    }

    fn assert_tree_does_not_contain(root: &Path, needle: &[u8]) {
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).expect("inspect token leak test path");
            if metadata.is_dir() {
                for entry in fs::read_dir(&path).expect("read token leak test directory") {
                    pending.push(entry.expect("read token leak test entry").path());
                }
            } else if metadata.is_file() {
                let bytes = fs::read(&path).expect("read token leak test file");
                assert!(
                    !bytes.windows(needle.len()).any(|window| window == needle),
                    "publish token leaked into '{}'",
                    path.display()
                );
            }
        }
    }

    fn seed_registry_artifact(
        cache_dir: &Path,
        artifact: &PackageArtifact,
        dependencies: Vec<PackageDependency>,
    ) -> (RegistryManifest, PathBuf, String) {
        let manifest = RegistryManifest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            source: format!(
                "https://registry.example/v1/artifacts/{}-{}-{}.tar.zst",
                artifact.name, artifact.version, artifact.checksum
            ),
            checksum: artifact.checksum.clone(),
            dependencies,
        };
        let plan = plan_registry_download(
            cache_dir,
            &manifest,
            None,
            RegistryFetchPolicy::default(),
            Span::default(),
        )
        .expect("plan seeded registry artifact");
        fs::create_dir_all(&plan.target_dir).expect("create seeded cache target");
        fs::copy(&artifact.path, plan.target_dir.join(REGISTRY_ARTIFACT_FILE))
            .expect("copy seeded registry archive");
        unpack_package_archive(
            &artifact.path,
            &plan.target_dir.join(REGISTRY_UNPACKED_DIR),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect("unpack seeded registry archive");
        fs::write(
            plan.target_dir.join(REGISTRY_CHECKSUM_FILE),
            format!("{}\n", artifact.checksum),
        )
        .expect("write seeded archive checksum");
        let tree = package_archive_tree_sha256(
            &artifact.path,
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect("hash seeded registry archive");
        fs::write(
            plan.target_dir.join(REGISTRY_TREE_CHECKSUM_FILE),
            format!("sha256-{tree}\n"),
        )
        .expect("write seeded tree checksum");
        let cache_key = plan
            .target_dir
            .file_name()
            .and_then(|value| value.to_str())
            .expect("seeded cache key")
            .to_string();
        (
            manifest,
            plan.target_dir.join(REGISTRY_UNPACKED_DIR),
            cache_key,
        )
    }

    #[test]
    fn registry_download_plan_is_bounded_and_cache_aware() {
        let manifest = RegistryManifest {
            name: "math".to_string(),
            version: "1.2.3".to_string(),
            source: "https://registry.example/math/1.2.3.tar.zst".to_string(),
            checksum: format!("sha256-{}", "b".repeat(64)),
            dependencies: Vec::new(),
        };
        let cache = temp_path("registry-plan");
        let policy = RegistryFetchPolicy::default();
        let reuse = plan_registry_download(
            &cache,
            &manifest,
            Some(&manifest.checksum),
            policy,
            Span::default(),
        )
        .expect("reuse plan");
        assert_eq!(reuse.action, RegistryCacheAction::ReuseVerified);
        assert_eq!(reuse.policy.max_attempts, 3);
        assert!(reuse.target_dir.ends_with(
            PathBuf::from("packages")
                .join("math")
                .join(format!("math-1.2.3-sha256-{}", "b".repeat(64)))
        ));
        assert!(reuse
            .temporary_dir
            .starts_with(cache.join(".registry-downloads")));

        let refresh = plan_registry_download(&cache, &manifest, None, policy, Span::default())
            .expect("refresh plan");
        assert_eq!(refresh.action, RegistryCacheAction::DownloadAndReplace);
        assert_ne!(refresh.target_dir, refresh.temporary_dir);
        let second_refresh =
            plan_registry_download(&cache, &manifest, None, policy, Span::default())
                .expect("second refresh plan");
        assert_ne!(
            refresh.temporary_dir, second_refresh.temporary_dir,
            "concurrent registry downloads must not share a temporary directory"
        );

        let err = plan_registry_download(
            &cache,
            &manifest,
            None,
            RegistryFetchPolicy {
                max_attempts: 0,
                ..policy
            },
            Span::default(),
        )
        .expect_err("zero retry attempts should fail");
        assert_eq!(err.code.as_deref(), Some("invalid_fetch_policy"));

        for policy in [
            RegistryFetchPolicy {
                max_attempts: 9,
                ..policy
            },
            RegistryFetchPolicy {
                max_download_bytes: 32_000_001,
                ..policy
            },
        ] {
            let err = plan_registry_download(&cache, &manifest, None, policy, Span::default())
                .expect_err("excessive registry resource policy should fail");
            assert_eq!(err.code.as_deref(), Some("invalid_fetch_policy"));
        }
    }

    #[test]
    fn static_index_resolves_https_urls_and_sorts_versions() {
        let index = parse_registry_index(
            r#"
name = "math"

[[version]]
version = "1.2.3"
url = "../packages/math-1.2.3.tar.zst"
checksum = "sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[[version]]
version = "2.0.0"
url = "https://cdn.example/math-2.0.0.tar.zst"
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
            "https://registry.example/packages/math-1.2.3.tar.zst"
        );
    }

    #[test]
    fn signed_empty_registry_index_is_valid_after_the_last_version_is_yanked() {
        let index = parse_registry_index(
            "name = \"math\"\n",
            "https://registry.example/v1/packages/math/index.toml",
            Span::default(),
        )
        .expect("a complete signed index may contain no selectable versions");
        assert_eq!(index.name, "math");
        assert!(index.versions.is_empty());

        assert_eq!(
            yank_http_status_error(401, "math", "1.0.0").code.as_deref(),
            Some("registry_yank_unauthorized")
        );
        assert_eq!(
            yank_http_status_error(404, "math", "1.0.0").code.as_deref(),
            Some("registry_version_not_found")
        );
        assert_eq!(
            yank_http_status_error(429, "math", "1.0.0").code.as_deref(),
            Some("registry_yank_limited")
        );
    }

    #[test]
    fn registry_rejects_http_and_duplicate_index_versions() {
        let manifest = RegistryManifest {
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            source: "http://registry.example/math.tar.zst".to_string(),
            checksum: "sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            dependencies: Vec::new(),
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
url = "math.tar.zst"
checksum = "sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[[version]]
version = "1.0.0"
url = "math-copy.tar.zst"
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
    fn deadline_aware_streams_do_not_write_a_chunk_after_expiry() {
        struct DelayedReader {
            deadline: Instant,
        }

        impl Read for DelayedReader {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let delay = self
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_add(Duration::from_millis(1));
                thread::sleep(delay);
                output[0] = b'x';
                Ok(1)
            }
        }

        let deadline = Instant::now() + Duration::from_millis(20);
        let mut output = Vec::new();
        let err = read_limited_until(
            DelayedReader { deadline },
            &mut output,
            8,
            Some(deadline),
            Span::default(),
        )
        .expect_err("a response chunk returned after the deadline must be rejected");
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));
        assert!(output.is_empty());

        let deadline = Instant::now() + Duration::from_millis(20);
        let err = stream_sha256_until(
            DelayedReader { deadline },
            &mut output,
            8,
            Some(deadline),
            Span::default(),
        )
        .expect_err("an artifact chunk returned after the deadline must not be written");
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));
        assert!(output.is_empty());
    }

    #[test]
    fn retry_loop_stops_at_the_configured_attempt_count() {
        let attempts = AtomicUsize::new(0);
        let policy = RegistryFetchPolicy {
            max_attempts: 3,
            ..RegistryFetchPolicy::default()
        };
        let err = retry_registry_request(
            policy,
            Instant::now() + Duration::from_secs(1),
            Span::default(),
            |_| {
                attempts.fetch_add(1, AtomicOrdering::Relaxed);
                Err::<(), _>(FetchAttemptError::Retry("temporary".to_string()))
            },
        )
        .expect_err("bounded retries must eventually fail");
        assert_eq!(attempts.load(AtomicOrdering::Relaxed), 3);
        assert_eq!(err.code.as_deref(), Some("registry_fetch_failed"));
    }

    #[test]
    fn registry_http_client_reuses_only_when_cached_timeouts_are_not_wider() {
        let cached = RegistryFetchPolicy::default();
        let exact = cached;
        let wider = RegistryFetchPolicy {
            connect_timeout_ms: cached.connect_timeout_ms + 1,
            read_timeout_ms: cached.read_timeout_ms + 1,
            ..cached
        };
        let shorter_connect = RegistryFetchPolicy {
            connect_timeout_ms: cached.connect_timeout_ms - 1,
            ..cached
        };
        let shorter_read = RegistryFetchPolicy {
            read_timeout_ms: cached.read_timeout_ms - 1,
            ..cached
        };

        assert_eq!(
            registry_http_client_policy_action(cached, exact),
            RegistryHttpClientPolicyAction::ReuseCached
        );
        assert_eq!(
            registry_http_client_policy_action(cached, wider),
            RegistryHttpClientPolicyAction::ReuseCached
        );
        assert_eq!(
            registry_http_client_policy_action(cached, shorter_connect),
            RegistryHttpClientPolicyAction::BuildForActivePolicy
        );
        assert_eq!(
            registry_http_client_policy_action(cached, shorter_read),
            RegistryHttpClientPolicyAction::BuildForActivePolicy
        );

        let origin = "https://registry-deadline.example/";
        let guard = TestRegistryHttpAgentGuard::install(
            origin.to_string(),
            ureq::AgentBuilder::new().build(),
        );
        let client = RegistryHttpClient::new(cached, origin);

        let exact_client = client.for_policy(exact);
        assert!(Arc::ptr_eq(&client.inner, &exact_client.inner));
        let _ = exact_client.agent();
        assert_eq!(guard.matched_client_count(), 1);

        let wider_client = client.for_policy(wider);
        assert!(Arc::ptr_eq(&client.inner, &wider_client.inner));
        let _ = wider_client.agent();
        assert_eq!(
            guard.matched_client_count(),
            1,
            "same or wider active timeouts must keep the cached connection pool"
        );

        let shorter_connect_client = client.for_policy(shorter_connect);
        assert!(!Arc::ptr_eq(&client.inner, &shorter_connect_client.inner));
        let _ = shorter_connect_client.agent();
        assert_eq!(guard.matched_client_count(), 2);

        let shorter_read_client = client.for_policy(shorter_read);
        assert!(!Arc::ptr_eq(&client.inner, &shorter_read_client.inner));
        let _ = shorter_read_client.agent();
        assert_eq!(
            guard.matched_client_count(),
            3,
            "each fetch with a stricter active timeout must get a correctly configured client"
        );
    }

    #[test]
    fn registry_operation_deadline_fails_before_retry_or_solver_work() {
        let expired = Instant::now() - Duration::from_millis(1);
        let err =
            registry_fetch_policy_for_deadline(expired, MAX_REGISTRY_INDEX_BYTES, Span::default())
                .expect_err("expired fetch budget must fail immediately");
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));

        let attempts = AtomicUsize::new(0);
        let err = retry_registry_request(
            RegistryFetchPolicy::default(),
            expired,
            Span::default(),
            |_| {
                attempts.fetch_add(1, AtomicOrdering::Relaxed);
                Ok(())
            },
        )
        .expect_err("an expired request must stop before the first HTTP attempt");
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));
        assert_eq!(attempts.load(AtomicOrdering::Relaxed), 0);

        let registry = PackageRegistry {
            url: "https://registry.example/v1/".to_string(),
            public_key: format!("ed25519-{}", "0".repeat(64)),
        };
        let mut solver = RegistryGraphSolver::with_deadline(&registry, expired);
        let err = solver
            .solve(&[PackageDependency {
                name: "math".to_string(),
                version: "^1.0.0".to_string(),
                source: None,
                checksum: None,
            }])
            .expect_err("expired solver budget must fail before fetching");
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));
        assert_eq!(solver.steps, 0);
    }

    #[test]
    fn retry_backoff_never_sleeps_past_the_absolute_deadline() {
        let now = Instant::now();
        let err = registry_retry_delay(1, now, now + Duration::from_millis(20), Span::default())
            .expect_err("retry backoff must yield to the operation deadline");
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));
        assert_eq!(
            registry_retry_delay(2, now, now + Duration::from_millis(100), Span::default(),)
                .expect("retry delay fits in the remaining budget"),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn fatal_fetch_errors_are_not_retried() {
        let attempts = AtomicUsize::new(0);
        let err = retry_registry_request(
            RegistryFetchPolicy::default(),
            Instant::now() + Duration::from_secs(1),
            Span::default(),
            |_| {
                attempts.fetch_add(1, AtomicOrdering::Relaxed);
                Err::<(), _>(FetchAttemptError::Fatal(KuError::package(
                    "checksum_mismatch",
                    "bad checksum",
                    Span::default(),
                )))
            },
        )
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

        let err = install_immutable_registry_cache(
            &source,
            &target,
            package_operation_deadline(),
            Span::default(),
        )
        .expect_err("content-addressed cache must never replace an existing directory");
        assert_eq!(err.code.as_deref(), Some("registry_cache_conflict"));
        assert!(source.exists());
        assert_eq!(
            fs::read(target.join("old")).expect("read old cache"),
            b"old"
        );
        let expired_target = root.join("expired-target");
        let expired_error = install_immutable_registry_cache(
            &source,
            &expired_target,
            Instant::now() - Duration::from_millis(1),
            Span::default(),
        )
        .expect_err("an expired operation must not install a new immutable cache");
        assert_eq!(
            expired_error.code.as_deref(),
            Some("registry_resolve_timeout")
        );
        assert!(source.is_dir());
        assert!(!expired_target.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_archive_unpack_accepts_safe_tar_zst() {
        let root = temp_path("archive-safe");
        fs::create_dir_all(&root).expect("create archive temp root");
        let archive = root.join("math.tar.zst");
        write_archive(
            &archive,
            &archive_bytes(&[
                ("math/ku.mod", b"name = \"math\"\nversion = \"1.0.0\"\n"),
                ("math/src/main.ku", b"fn main() {}\n"),
                ("math/README.md", b"# math\n"),
            ]),
        );
        let output = root.join("out");
        unpack_package_archive(
            &archive,
            &output,
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect("safe archive should unpack");
        assert_eq!(
            fs::read_to_string(output.join("src").join("main.ku")).expect("read unpacked file"),
            "fn main() {}\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_identity_unpack_requires_the_exact_name_version_root() {
        let root = temp_path("archive-identity-root");
        fs::create_dir_all(&root).expect("create archive identity temp root");
        let archive = root.join("wrong-root.tar.zst");
        write_archive(
            &archive,
            &archive_bytes(&[
                (
                    "other-1.0.0/ku.mod",
                    b"name = \"math\"\nversion = \"1.0.0\"\n",
                ),
                ("other-1.0.0/src/main.ku", b"fn main() {}\n"),
            ]),
        );

        let err = unpack_package_archive_for_identity(
            &archive,
            &root.join("identity-out"),
            PackageArchivePolicy::default(),
            "math",
            "1.0.0",
            None,
            Span::default(),
        )
        .expect_err("registry and pack verification must reject a mismatched tar root");
        assert_eq!(err.code.as_deref(), Some("package_identity_mismatch"));

        unpack_package_archive(
            &archive,
            &root.join("generic-out"),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect("the public generic unpack API must remain root-agnostic");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_publish_archive_deadline_is_checked_before_unpacking() {
        let root = temp_path("archive-expired-deadline");
        fs::create_dir_all(&root).expect("create expired archive test root");
        let archive = root.join("math.tar.zst");
        write_archive(
            &archive,
            &archive_bytes(&[(
                "math-1.0.0/ku.mod",
                b"name = \"math\"\nversion = \"1.0.0\"\n",
            )]),
        );
        let err = validate_registry_publish_archive(
            &archive,
            &root.join("out"),
            "math",
            "1.0.0",
            Instant::now() - Duration::from_millis(1),
            Span::default(),
        )
        .expect_err("expired publish validation must stop before decompression");
        assert_eq!(err.code.as_deref(), Some("package_archive_timeout"));
        assert!(!root.join("out").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_archive_rejects_zstd_frames_above_the_window_limit() {
        let root = temp_path("archive-zstd-window");
        fs::create_dir_all(&root).expect("create zstd window test root");
        let archive = root.join("large-window.tar.zst");
        // Minimal zstd frame: ordinary frame descriptor, window descriptor with
        // exponent 18 (windowLog 28, 256 MiB), followed by an empty last block.
        fs::write(
            &archive,
            [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x90, 0x01, 0x00, 0x00],
        )
        .expect("write oversized-window zstd frame");
        let err = unpack_package_archive(
            &archive,
            &root.join("out"),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect_err("zstd window above 2^27 must be rejected");
        assert_eq!(err.code.as_deref(), Some("invalid_package_archive"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_archive_rejects_unsafe_paths_missing_manifest_and_multiple_roots() {
        let root = temp_path("archive-reject");
        fs::create_dir_all(&root).expect("create archive temp root");

        let mut archive_root = None;
        let err = validate_archive_entry_path(
            Path::new("math/../evil"),
            &mut archive_root,
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect_err("parent path must be rejected");
        assert_eq!(err.code.as_deref(), Some("unsafe_archive_path"));
        for component in ["bad?.ku", "bad*.ku", "bad|name", "bad<name>", "bad\"name"] {
            let err = validate_portable_package_component(component, Span::default())
                .expect_err("Windows-invalid path characters must be rejected on every host");
            assert_eq!(err.code.as_deref(), Some("unsafe_archive_path"));
        }

        let missing_manifest = root.join("missing.tar.zst");
        write_archive(
            &missing_manifest,
            &archive_bytes(&[("math/src/main.ku", b"fn main() {}\n")]),
        );
        let err = unpack_package_archive(
            &missing_manifest,
            &root.join("missing-out"),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect_err("missing ku.mod must be rejected");
        assert_eq!(err.code.as_deref(), Some("missing_package_manifest"));

        let multiple_roots = root.join("multi.tar.zst");
        write_archive(
            &multiple_roots,
            &archive_bytes(&[
                ("math/ku.mod", b"name = \"math\"\n"),
                ("other/ku.mod", b"name = \"other\"\n"),
            ]),
        );
        let err = unpack_package_archive(
            &multiple_roots,
            &root.join("multi-out"),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect_err("multiple roots must be rejected");
        assert_eq!(err.code.as_deref(), Some("invalid_package_archive"));

        let case_conflict = root.join("case-conflict.tar.zst");
        write_archive(
            &case_conflict,
            &archive_bytes(&[
                ("math/ku.mod", b"name = \"math\"\n"),
                ("math/src/A.ku", b"fn A() {}\n"),
                ("math/src/a.ku", b"fn B() {}\n"),
            ]),
        );
        let err = unpack_package_archive(
            &case_conflict,
            &root.join("case-conflict-out"),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect_err("case-conflicting archive paths must be rejected");
        assert_eq!(err.code.as_deref(), Some("duplicate_archive_path"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_archive_rejects_links_and_resource_limits() {
        let root = temp_path("archive-links");
        fs::create_dir_all(&root).expect("create archive temp root");

        let symlink_archive = root.join("link.tar.zst");
        write_archive(
            &symlink_archive,
            &archive_with_special_entry("math/link", "ku.mod", tar::EntryType::Symlink),
        );
        let err = unpack_package_archive(
            &symlink_archive,
            &root.join("link-out"),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect_err("symlink must be rejected");
        assert_eq!(err.code.as_deref(), Some("unsupported_archive_entry"));

        let hardlink_archive = root.join("hardlink.tar.zst");
        write_archive(
            &hardlink_archive,
            &archive_with_special_entry("math/link", "ku.mod", tar::EntryType::Link),
        );
        let err = unpack_package_archive(
            &hardlink_archive,
            &root.join("hardlink-out"),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect_err("hardlink must be rejected");
        assert_eq!(err.code.as_deref(), Some("unsupported_archive_entry"));

        let oversized_file = root.join("oversized.tar.zst");
        write_archive(
            &oversized_file,
            &archive_bytes(&[
                ("math/ku.mod", b"name = \"math\"\n"),
                ("math/src/main.ku", b"12345"),
            ]),
        );
        let err = unpack_package_archive(
            &oversized_file,
            &root.join("oversized-out"),
            PackageArchivePolicy {
                max_file_bytes: 4,
                ..PackageArchivePolicy::default()
            },
            Span::default(),
        )
        .expect_err("oversized file must be rejected");
        assert_eq!(err.code.as_deref(), Some("package_archive_limit"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_archive_counts_empty_directories_against_entry_limit() {
        let root = temp_path("archive-directory-limit");
        fs::create_dir_all(&root).expect("create test root");
        let archive = root.join("directories.tar.zst");
        write_archive(&archive, &archive_with_directories(3));
        let policy = PackageArchivePolicy {
            max_files: 4,
            ..PackageArchivePolicy::default()
        };
        let unpack_error =
            unpack_package_archive(&archive, &root.join("unpacked"), policy, Span::default())
                .expect_err("empty directories must consume the archive entry budget");
        assert_eq!(unpack_error.code.as_deref(), Some("package_archive_limit"));
        let hash_error = package_archive_tree_sha256(&archive, policy, Span::default())
            .expect_err("tree hashing must enforce the same entry budget");
        assert_eq!(hash_error.code.as_deref(), Some("package_archive_limit"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn verified_cache_is_reused_without_network_access() {
        let root = temp_path("registry-cache-reuse");
        let cache_dir = root.clone();
        let archive = archive_bytes(&[
            ("math/ku.mod", b"name = \"math\"\nversion = \"1.0.0\"\n"),
            ("math/src/math.ku", b"fn Value(): int { return 1 }\n"),
        ]);
        let checksum = sha256_checksum(&archive);
        let target = cache_dir
            .join(PACKAGE_CACHE_DIR)
            .join("math")
            .join(registry_cache_key_for_identity("math", "1.0.0", &checksum));
        fs::create_dir_all(&target).expect("create cache");
        fs::write(target.join(REGISTRY_ARTIFACT_FILE), &archive).expect("write artifact");
        fs::create_dir_all(target.join(REGISTRY_UNPACKED_DIR)).expect("create package root");
        fs::write(
            target.join(REGISTRY_UNPACKED_DIR).join(MANIFEST_FILE),
            b"name = \"math\"\nversion = \"1.0.0\"\n",
        )
        .expect("write unpacked manifest");
        fs::create_dir_all(target.join(REGISTRY_UNPACKED_DIR).join("src"))
            .expect("create package src");
        fs::write(
            target
                .join(REGISTRY_UNPACKED_DIR)
                .join("src")
                .join("math.ku"),
            b"fn Value(): int { return 1 }\n",
        )
        .expect("write package source");
        let tree = package_tree_sha256(
            &target.join(REGISTRY_UNPACKED_DIR),
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect("hash package tree");
        fs::write(
            target.join(REGISTRY_TREE_CHECKSUM_FILE),
            format!("sha256-{tree}\n"),
        )
        .expect("write tree checksum");
        fs::write(target.join(REGISTRY_CHECKSUM_FILE), format!("{checksum}\n"))
            .expect("write artifact checksum");
        let plan = RegistryDownloadPlan {
            cache_dir,
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            url: "https://unreachable.invalid/math.tar.zst".to_string(),
            checksum,
            target_dir: target.clone(),
            temporary_dir: root
                .join(".registry-downloads")
                .join(format!("math-1.0.0-{}", operation_nonce())),
            action: RegistryCacheAction::ReuseVerified,
            policy: RegistryFetchPolicy::default(),
        };

        let package_root =
            execute_registry_download(&plan, Span::default()).expect("verified cache reuse");
        assert_eq!(package_root, target.join(REGISTRY_UNPACKED_DIR));
        let offline_root = execute_registry_download_until(
            &plan,
            Span::default(),
            package_operation_deadline(),
            RegistryNetworkPolicy::Deny,
        )
        .expect("offline mode must reuse a verified cache under the package lock");
        assert_eq!(offline_root, package_root);

        fs::write(
            target
                .join(REGISTRY_UNPACKED_DIR)
                .join("src")
                .join("math.ku"),
            b"fn Value(): int { return 999 }\n",
        )
        .expect("tamper cached source");
        let offline_err = execute_registry_download_until(
            &plan,
            Span::default(),
            package_operation_deadline(),
            RegistryNetworkPolicy::Deny,
        )
        .expect_err("offline mode must reject a tampered cache without downloading");
        assert_eq!(offline_err.code.as_deref(), Some("offline_cache_miss"));
        assert!(
            !plan.temporary_dir.exists(),
            "offline verification must return before staging or network work"
        );
        fs::remove_dir_all(&target).expect("remove cache to test offline miss");
        let missing_err = execute_registry_download_until(
            &plan,
            Span::default(),
            package_operation_deadline(),
            RegistryNetworkPolicy::Deny,
        )
        .expect_err("offline mode must reject a missing cache without downloading");
        assert_eq!(missing_err.code.as_deref(), Some("offline_cache_miss"));
        assert!(!plan.temporary_dir.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn online_registry_cache_repair_is_locked_and_downloaded_once() {
        let root = temp_path("registry-cache-repair");
        let archive = archive_bytes(&[
            (
                "math-1.0.0/ku.mod",
                b"name = \"math\"\nversion = \"1.0.0\"\n",
            ),
            ("math-1.0.0/src/math.ku", b"fn Value(): int { return 1 }\n"),
        ]);
        let checksum = sha256_checksum(&archive);
        let signing_key = SigningKey::from_bytes(&[19u8; 32]);
        let (registry, agent) =
            TestHttpsRegistry::start_for(signing_key, "unused", "math", "1.0.0", Vec::new());
        registry
            .state
            .lock()
            .expect("test registry state mutex poisoned")
            .artifact = Some(archive);
        let manifest = RegistryManifest {
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            source: format!(
                "{}artifacts/math-1.0.0-{checksum}.tar.zst",
                registry.base_url
            ),
            checksum,
            dependencies: Vec::new(),
        };
        let plan = plan_registry_download(
            &root.join("cache"),
            &manifest,
            None,
            RegistryFetchPolicy::default(),
            Span::default(),
        )
        .expect("plan registry cache repair");
        let _agent_guard = TestRegistryHttpAgentGuard::install(registry.base_url.clone(), agent);
        execute_registry_download(&plan, Span::default()).expect("populate verified cache");
        registry
            .state
            .lock()
            .expect("test registry state mutex poisoned")
            .artifact_reads = 0;

        let expired_valid_error = execute_registry_download_until(
            &plan,
            Span::default(),
            Instant::now() - Duration::from_millis(1),
            RegistryNetworkPolicy::Allow,
        )
        .expect_err("an expired operation must not return a verified cache fast path");
        assert_eq!(
            expired_valid_error.code.as_deref(),
            Some("registry_resolve_timeout")
        );
        assert_eq!(
            registry
                .state
                .lock()
                .expect("test registry state mutex poisoned")
                .artifact_reads,
            0,
            "an expired cache fast path must not start a network request"
        );

        let source_path = plan
            .target_dir
            .join(REGISTRY_UNPACKED_DIR)
            .join("src")
            .join("math.ku");
        let original = fs::read(&source_path).expect("read installed source");
        let tampered = b"fn Value(): int { return 9 }\n";
        assert_eq!(original.len(), tampered.len(), "tamper must preserve size");
        fs::write(&source_path, tampered).expect("tamper installed source at the same size");
        let package_entries_before = fs::read_dir(
            plan.target_dir
                .parent()
                .expect("registry target package root"),
        )
        .expect("read package cache before offline verification")
        .map(|entry| entry.expect("read package cache entry").file_name())
        .collect::<HashSet<_>>();
        let offline_error = execute_registry_download_until(
            &plan,
            Span::default(),
            package_operation_deadline(),
            RegistryNetworkPolicy::Deny,
        )
        .expect_err("offline mode must reject, not repair, a corrupted cache");
        assert_eq!(offline_error.code.as_deref(), Some("offline_cache_miss"));
        assert_eq!(
            fs::read(&source_path).expect("re-read offline-corrupt source"),
            tampered,
            "offline verification must not change corrupted cache bytes"
        );
        let package_entries_after = fs::read_dir(
            plan.target_dir
                .parent()
                .expect("registry target package root"),
        )
        .expect("read package cache after offline verification")
        .map(|entry| entry.expect("read package cache entry").file_name())
        .collect::<HashSet<_>>();
        assert_eq!(package_entries_after, package_entries_before);

        let expired_repair_error = execute_registry_download_until(
            &plan,
            Span::default(),
            Instant::now() - Duration::from_millis(1),
            RegistryNetworkPolicy::Allow,
        )
        .expect_err("an expired online repair must stop before quarantine");
        assert_eq!(
            expired_repair_error.code.as_deref(),
            Some("registry_resolve_timeout")
        );
        assert_eq!(
            fs::read(&source_path).expect("re-read expired-repair source"),
            tampered,
            "an expired online repair must not move or rewrite corrupted cache bytes"
        );
        let package_entries_after_expired_repair = fs::read_dir(
            plan.target_dir
                .parent()
                .expect("registry target package root"),
        )
        .expect("read package cache after expired repair")
        .map(|entry| entry.expect("read package cache entry").file_name())
        .collect::<HashSet<_>>();
        assert_eq!(package_entries_after_expired_repair, package_entries_before);

        let plan = Arc::new(plan);
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let plan = Arc::clone(&plan);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                execute_registry_download(&plan, Span::default())
            }));
        }
        barrier.wait();
        for worker in workers {
            assert_eq!(
                worker
                    .join()
                    .expect("registry repair worker panicked")
                    .expect("registry repair worker failed"),
                plan.target_dir.join(REGISTRY_UNPACKED_DIR)
            );
        }
        let state = registry
            .state
            .lock()
            .expect("test registry state mutex poisoned");
        assert_eq!(
            state.artifact_reads, 1,
            "the package OS lock must serialize repair to one download"
        );
        drop(state);
        assert_eq!(
            fs::read(&source_path).expect("read repaired source"),
            original
        );
        assert!(
            installed_registry_cache_is_verified_with_budget(&plan, Span::default(), None)
                .expect("verify repaired registry cache")
        );
        let quarantines = fs::read_dir(
            plan.target_dir
                .parent()
                .expect("registry target package root"),
        )
        .expect("read repaired package cache")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(registry_quarantine_name_is_valid)
        })
        .count();
        assert_eq!(quarantines, 1);
        for _ in 1..MAX_REGISTRY_QUARANTINES_PER_PACKAGE {
            let quarantine = registry_quarantine_path(&plan);
            fs::create_dir(&quarantine).expect("create retained repair quarantine");
            fs::write(quarantine.join("retained"), b"do not discard")
                .expect("write retained quarantine marker");
        }
        fs::write(&source_path, tampered).expect("tamper cache after reaching quarantine cap");
        let capped_error = execute_registry_download(&plan, Span::default())
            .expect_err("repair must stop when the package quarantine cap is reached");
        assert_eq!(
            capped_error.code.as_deref(),
            Some("registry_quarantine_limit")
        );
        assert!(capped_error.to_string().contains("ku package gc"));
        assert_eq!(
            fs::read(&source_path).expect("read cap-rejected target"),
            tampered,
            "a cap failure must not rename or rewrite the active target"
        );
        assert!(!plan.temporary_dir.exists());
        assert_eq!(
            registry
                .state
                .lock()
                .expect("read registry state")
                .artifact_reads,
            1,
            "reaching the quarantine cap must not start another download"
        );
        let offline_error = execute_registry_download_until(
            &plan,
            Span::default(),
            package_operation_deadline(),
            RegistryNetworkPolicy::Deny,
        )
        .expect_err("offline mode still reports a corrupt-cache miss, not a repair limit");
        assert_eq!(offline_error.code.as_deref(), Some("offline_cache_miss"));
        assert_eq!(
            fs::read(&source_path).expect("read offline cap-rejected target"),
            tampered
        );
        fs::write(&source_path, &original).expect("restore valid source for offline-cap test");
        let offline_valid = execute_registry_download_until(
            &plan,
            Span::default(),
            package_operation_deadline(),
            RegistryNetworkPolicy::Deny,
        )
        .expect("the quarantine cap must not block reuse of a valid offline cache");
        assert_eq!(offline_valid, plan.target_dir.join(REGISTRY_UNPACKED_DIR));
        let retained = fs::read_dir(plan.target_dir.parent().expect("package cache root"))
            .expect("read retained package entries")
            .map(|entry| entry.expect("read retained entry").file_name())
            .filter(|name| {
                name.as_encoded_bytes()
                    .starts_with(REGISTRY_QUARANTINE_PREFIX.as_bytes())
            })
            .count();
        assert_eq!(retained, MAX_REGISTRY_QUARANTINES_PER_PACKAGE);
        let _ = fs::remove_dir_all(root);
    }

    fn corrupt_cache_plan_for_quarantine_test(cache: &Path, version: &str) -> RegistryDownloadPlan {
        let checksum = sha256_checksum(version.as_bytes());
        let plan = RegistryDownloadPlan {
            cache_dir: cache.to_path_buf(),
            name: "math".to_string(),
            version: version.to_string(),
            url: "https://unreachable.invalid/math.tar.zst".to_string(),
            checksum: checksum.clone(),
            target_dir: cache
                .join(PACKAGE_CACHE_DIR)
                .join("math")
                .join(registry_cache_key_for_identity("math", version, &checksum)),
            temporary_dir: cache
                .join(".registry-downloads")
                .join(format!("math-{version}-{}", operation_nonce())),
            action: RegistryCacheAction::DownloadAndReplace,
            policy: RegistryFetchPolicy::default(),
        };
        fs::create_dir_all(&plan.target_dir).expect("create corrupt cache fixture");
        fs::write(
            plan.target_dir.join("corrupt"),
            b"preserve until bounded repair",
        )
        .expect("write corrupt cache marker");
        plan
    }

    #[test]
    fn registry_quarantine_capacity_is_shared_across_concurrent_content_keys() {
        let root = temp_path("registry-quarantine-concurrent-keys");
        let cache = root.join("cache");
        let plans = (0..8)
            .map(|version| {
                corrupt_cache_plan_for_quarantine_test(&cache, &format!("1.0.{version}"))
            })
            .collect::<Vec<_>>();
        for _ in 1..MAX_REGISTRY_QUARANTINES_PER_PACKAGE {
            fs::create_dir(registry_quarantine_path(&plans[0]))
                .expect("create retained package quarantine");
        }
        let barrier = Arc::new(Barrier::new(plans.len() + 1));
        let workers = plans
            .into_iter()
            .map(|plan| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let span = Span::default();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let cache_root =
                        open_registry_cache_directory(&plan.cache_dir, "test cache", span)
                            .expect("open concurrent test cache");
                    let layout = prepare_registry_cache_layout(&plan, cache_root, span)
                        .expect("open concurrent package layout");
                    let key = plan
                        .target_dir
                        .file_name()
                        .and_then(|name| name.to_str())
                        .expect("cache key");
                    let _install_lock = acquire_registry_install_lock_until(
                        &package_operation_lock_path(&plan.cache_dir, "registry", &plan.name, key),
                        span,
                        deadline,
                    )
                    .expect("acquire distinct content-key install lock");
                    let result = quarantine_corrupt_registry_cache_with_post_rename(
                        &plan,
                        &layout,
                        deadline,
                        span,
                        |_| {
                            assert!(
                                try_acquire_registry_install_lock(&package_operation_lock_path(
                                    &plan.cache_dir,
                                    "registry-repair",
                                    &plan.name,
                                    "quarantine",
                                ))
                                .expect("inspect held package repair lock")
                                .is_none(),
                                "the package lock must remain held through quarantine rename"
                            );
                            Ok(())
                        },
                    );
                    (plan.target_dir, result)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let mut repaired = 0usize;
        let mut refused = 0usize;
        for worker in workers {
            let (target, result) = worker
                .join()
                .expect("concurrent quarantine worker panicked");
            match result {
                Ok(quarantine) => {
                    repaired += 1;
                    assert!(!target.exists());
                    assert_eq!(
                        fs::read(quarantine.join("corrupt")).expect("read quarantined marker"),
                        b"preserve until bounded repair"
                    );
                }
                Err(error) => {
                    refused += 1;
                    assert_eq!(error.code.as_deref(), Some("registry_quarantine_limit"));
                    assert_eq!(
                        fs::read(target.join("corrupt")).expect("read refused target"),
                        b"preserve until bounded repair"
                    );
                }
            }
        }
        assert_eq!(
            repaired, 1,
            "different cache keys must share the remaining slot"
        );
        assert_eq!(refused, 7);
        let quarantines = fs::read_dir(cache.join(PACKAGE_CACHE_DIR).join("math"))
            .expect("read package quarantine count")
            .map(|entry| entry.expect("read package entry").file_name())
            .filter(|name| {
                name.as_encoded_bytes()
                    .starts_with(REGISTRY_QUARANTINE_PREFIX.as_bytes())
            })
            .count();
        assert_eq!(quarantines, MAX_REGISTRY_QUARANTINES_PER_PACKAGE);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_quarantine_capacity_scan_is_bounded_and_counts_unknown_prefix_entries() {
        let root = temp_path("registry-quarantine-capacity-scan");
        fs::create_dir_all(&root).expect("create capacity scan fixture");
        fs::write(root.join("ordinary"), b"retain").expect("create ordinary fixture entry");
        let mut short_budget = CacheGcScanBudget {
            remaining_entries: 1,
            deadline: package_operation_deadline(),
        };
        let error = ensure_registry_quarantine_capacity(&root, &mut short_budget, Span::default())
            .expect_err("repair must fail if enumeration cannot prove it reached EOF");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_quarantine_scan_limit")
        );
        assert_eq!(short_budget.remaining_entries, 0);
        for index in 0..MAX_REGISTRY_QUARANTINES_PER_PACKAGE {
            fs::write(
                root.join(format!("{REGISTRY_QUARANTINE_PREFIX}unknown-{index}")),
                b"unknown",
            )
            .expect("write unknown reserved-prefix entry");
        }
        let mut full_budget =
            CacheGcScanBudget::for_registry_operation(package_operation_deadline());
        let error = ensure_registry_quarantine_capacity(&root, &mut full_budget, Span::default())
            .expect_err("malformed or non-directory quarantine entries also consume capacity");
        assert_eq!(error.code.as_deref(), Some("registry_quarantine_limit"));
        assert_eq!(
            fs::read_dir(&root)
                .expect("read untouched scan fixture")
                .count(),
            MAX_REGISTRY_QUARANTINES_PER_PACKAGE + 1
        );
        let mut expired_budget = CacheGcScanBudget {
            remaining_entries: MAX_REGISTRY_QUARANTINE_SCAN_ENTRIES,
            deadline: Instant::now() - Duration::from_millis(1),
        };
        let error =
            ensure_registry_quarantine_capacity(&root, &mut expired_budget, Span::default())
                .expect_err("the package operation deadline governs capacity scanning");
        assert_eq!(error.code.as_deref(), Some("registry_resolve_timeout"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_quarantine_package_lock_obeys_the_existing_operation_deadline() {
        let root = temp_path("registry-quarantine-lock-deadline");
        let plan = corrupt_cache_plan_for_quarantine_test(&root.join("cache"), "1.0.0");
        let span = Span::default();
        let cache_root = open_registry_cache_directory(&plan.cache_dir, "test cache", span)
            .expect("open lock-deadline cache");
        let layout =
            prepare_registry_cache_layout(&plan, cache_root, span).expect("open cache layout");
        let repair_lock = acquire_registry_install_lock_until(
            &package_operation_lock_path(
                &plan.cache_dir,
                "registry-repair",
                &plan.name,
                "quarantine",
            ),
            span,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("hold package repair lock");
        let started = Instant::now();
        let error = quarantine_corrupt_registry_cache(
            &plan,
            &layout,
            started + Duration::from_millis(30),
            span,
        )
        .expect_err("waiting for a package repair lock must consume the caller deadline");
        assert_eq!(error.code.as_deref(), Some("registry_resolve_timeout"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            fs::read(plan.target_dir.join("corrupt")).expect("read locked target"),
            b"preserve until bounded repair"
        );
        drop(repair_lock);
        drop(layout);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_cache_repair_rejects_unbounded_trees_before_rename_and_download() {
        let root = temp_path("registry-quarantine-tree-limits");
        let archive = archive_bytes(&[
            (
                "math-1.0.0/ku.mod",
                b"name = \"math\"\nversion = \"1.0.0\"\n",
            ),
            ("math-1.0.0/src/math.ku", b"fn Value(): int { return 1 }\n"),
        ]);
        let checksum = sha256_checksum(&archive);
        let (registry, agent) = TestHttpsRegistry::start_for(
            SigningKey::from_bytes(&[20u8; 32]),
            "unused",
            "math",
            "1.0.0",
            Vec::new(),
        );
        registry.state.lock().expect("open fixture state").artifact = Some(archive);
        let _agent_guard = TestRegistryHttpAgentGuard::install(registry.base_url.clone(), agent);
        let manifest = RegistryManifest {
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            source: format!(
                "{}artifacts/math-1.0.0-{checksum}.tar.zst",
                registry.base_url
            ),
            checksum,
            dependencies: Vec::new(),
        };
        for case in ["file-size", "entry-count", "nested-link"] {
            let plan = plan_registry_download(
                &root.join(case),
                &manifest,
                None,
                RegistryFetchPolicy::default(),
                Span::default(),
            )
            .expect("plan bounded-tree repair");
            fs::create_dir_all(&plan.target_dir).expect("create oversized corrupt target");
            let marker = plan.target_dir.join("corrupt");
            fs::write(&marker, b"retain this corrupt tree").expect("write corrupt marker");
            match case {
                "file-size" => fs::File::create(plan.target_dir.join("oversized"))
                    .expect("create oversized fixture")
                    .set_len(MAX_REGISTRY_DOWNLOAD_BYTES + 1)
                    .expect("set oversized length"),
                "entry-count" => {
                    for entry in 0..PackageArchivePolicy::default().max_files + 4 {
                        fs::write(plan.target_dir.join(format!("entry-{entry}")), b"")
                            .expect("write over-budget empty entry");
                    }
                }
                "nested-link" => {
                    let external = root.join("external-marker");
                    fs::write(&external, b"outside").expect("write external link marker");
                    #[cfg(unix)]
                    let linked =
                        std::os::unix::fs::symlink(&external, plan.target_dir.join("link")).is_ok();
                    #[cfg(windows)]
                    let linked =
                        std::os::windows::fs::symlink_file(&external, plan.target_dir.join("link"))
                            .is_ok();
                    if !linked {
                        eprintln!(
                            "skip quarantine nested-link fixture: symlink privilege unavailable"
                        );
                        continue;
                    }
                }
                _ => unreachable!(),
            }
            let error = execute_registry_download(&plan, Span::default())
                .expect_err("unbounded or unsafe trees must remain outside automatic repair");
            assert_eq!(error.code.as_deref(), Some("unsafe_cache_path"), "{case}");
            assert_eq!(
                fs::read(&marker).expect("read refused corrupt marker"),
                b"retain this corrupt tree"
            );
            assert_eq!(
                fs::read_dir(plan.target_dir.parent().expect("package root"))
                    .expect("read untouched package root")
                    .count(),
                1,
                "{case}: no quarantine may be created"
            );
            assert!(!plan.temporary_dir.exists());
        }
        assert_eq!(
            registry
                .state
                .lock()
                .expect("read fixture state")
                .artifact_reads,
            0,
            "rejected target trees must not trigger a new download"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_cache_repair_rejects_linked_layouts_before_external_writes() {
        let root = temp_path("registry-cache-linked-layout");
        let cache = root.join("cache");
        let external = root.join("external");
        fs::create_dir_all(&cache).expect("create cache root");
        fs::create_dir_all(&external).expect("create external root");
        let sentinel = external.join("sentinel");
        fs::write(&sentinel, b"unchanged").expect("write external sentinel");
        let checksum = format!("sha256-{}", "a".repeat(64));
        let plan = RegistryDownloadPlan {
            cache_dir: cache.clone(),
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            url: "https://unreachable.invalid/math.tar.zst".to_string(),
            checksum: checksum.clone(),
            target_dir: cache
                .join(PACKAGE_CACHE_DIR)
                .join("math")
                .join(registry_cache_key_for_identity("math", "1.0.0", &checksum)),
            temporary_dir: cache
                .join(".registry-downloads")
                .join(format!("math-1.0.0-{}", operation_nonce())),
            action: RegistryCacheAction::DownloadAndReplace,
            policy: RegistryFetchPolicy::default(),
        };
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&external, cache.join(PACKAGE_CACHE_DIR)).is_ok();
        #[cfg(windows)]
        let linked =
            std::os::windows::fs::symlink_dir(&external, cache.join(PACKAGE_CACHE_DIR)).is_ok();
        if linked {
            let error = execute_registry_download(&plan, Span::default())
                .expect_err("a linked packages root must fail before download or repair");
            assert_eq!(error.code.as_deref(), Some("unsafe_cache_path"));
            assert_eq!(
                fs::read(&sentinel).expect("read external sentinel"),
                b"unchanged"
            );
            assert!(
                !external.join("math").exists(),
                "linked cache roots must be rejected before creating external directories"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_cache_repair_rejects_linked_slot_and_staging_roots() {
        for internal_root in [".registry-slots", ".registry-downloads"] {
            let root = temp_path(&format!("registry-cache-linked-{internal_root}"));
            let cache = root.join("cache");
            let external = root.join("external");
            fs::create_dir_all(&cache).expect("create cache root");
            fs::create_dir_all(&external).expect("create external root");
            let sentinel = external.join("sentinel");
            fs::write(&sentinel, b"unchanged").expect("write external sentinel");
            let checksum = format!("sha256-{}", "c".repeat(64));
            let plan = RegistryDownloadPlan {
                cache_dir: cache.clone(),
                name: "math".to_string(),
                version: "1.0.0".to_string(),
                url: "https://unreachable.invalid/math.tar.zst".to_string(),
                checksum: checksum.clone(),
                target_dir: cache
                    .join(PACKAGE_CACHE_DIR)
                    .join("math")
                    .join(registry_cache_key_for_identity("math", "1.0.0", &checksum)),
                temporary_dir: cache
                    .join(".registry-downloads")
                    .join(format!("math-1.0.0-{}", operation_nonce())),
                action: RegistryCacheAction::DownloadAndReplace,
                policy: RegistryFetchPolicy::default(),
            };
            #[cfg(unix)]
            let linked = std::os::unix::fs::symlink(&external, cache.join(internal_root)).is_ok();
            #[cfg(windows)]
            let linked =
                std::os::windows::fs::symlink_dir(&external, cache.join(internal_root)).is_ok();
            if linked {
                let error = execute_registry_download(&plan, Span::default())
                    .expect_err("linked registry internal roots must fail before network work");
                assert_eq!(error.code.as_deref(), Some("unsafe_cache_path"));
                assert_eq!(
                    fs::read(&sentinel).expect("read external sentinel"),
                    b"unchanged"
                );
                assert_eq!(
                    fs::read_dir(&external)
                        .expect("read external root")
                        .filter_map(Result::ok)
                        .count(),
                    1,
                    "linked internal cache roots must not create external entries"
                );
                assert!(!plan.target_dir.exists());
                #[cfg(unix)]
                fs::remove_file(cache.join(internal_root)).expect("remove test directory symlink");
                #[cfg(windows)]
                fs::remove_dir(cache.join(internal_root)).expect("remove test directory symlink");
            }
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn registry_download_plan_rejects_non_derived_staging_path() {
        let root = temp_path("registry-cache-invalid-staging");
        fs::create_dir_all(&root).expect("create cache root");
        let checksum = format!("sha256-{}", "d".repeat(64));
        let plan = RegistryDownloadPlan {
            cache_dir: root.clone(),
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            url: "https://unreachable.invalid/math.tar.zst".to_string(),
            checksum: checksum.clone(),
            target_dir: root
                .join(PACKAGE_CACHE_DIR)
                .join("math")
                .join(registry_cache_key_for_identity("math", "1.0.0", &checksum)),
            temporary_dir: root.join("outside-staging"),
            action: RegistryCacheAction::DownloadAndReplace,
            policy: RegistryFetchPolicy::default(),
        };
        let error = execute_registry_download(&plan, Span::default())
            .expect_err("non-derived staging paths must fail before cache or network work");
        assert_eq!(error.code.as_deref(), Some("invalid_cache_path"));
        assert!(!root.join(PACKAGE_CACHE_DIR).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_temporary_cleanup_retains_data_after_staging_root_replacement() {
        let root = temp_path("registry-staging-root-replaced");
        let cache = root.join("cache");
        fs::create_dir_all(&cache).expect("create cache root");
        let cache_root =
            open_registry_cache_directory(&cache, "test registry cache root", Span::default())
                .expect("open cache root");
        let staging = ensure_registry_cache_child_directory(
            &cache_root,
            ".registry-downloads",
            "test registry staging root",
            Span::default(),
        )
        .expect("prepare staging root");
        let temporary_path = staging
            .path
            .join(format!("math-1.0.0-{}", operation_nonce()));
        fs::create_dir(&temporary_path).expect("create original temporary directory");
        fs::write(temporary_path.join("original"), b"original")
            .expect("write original temporary marker");
        let temporary_root = open_registry_cache_directory(
            &temporary_path,
            "test registry temporary directory",
            Span::default(),
        )
        .expect("open temporary directory");
        let temporary =
            TemporaryDirectory::new_registry(temporary_path.clone(), staging, temporary_root);
        temporary
            .validate_registry_location(Span::default())
            .expect("initial temporary location must verify");

        let displaced_staging = cache.join(".registry-downloads.displaced");
        if let Err(error) = fs::rename(cache.join(".registry-downloads"), &displaced_staging) {
            #[cfg(windows)]
            {
                assert_eq!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied,
                    "Windows must either block the held-parent replacement or report access denied"
                );
                temporary
                    .validate_registry_location(Span::default())
                    .expect("blocked parent replacement must preserve the temporary identity");
                drop(temporary);
                assert!(!temporary_path.exists());
                let _ = fs::remove_dir_all(root);
                return;
            }
            #[cfg(not(windows))]
            panic!("replace held staging root: {error}");
        }
        fs::create_dir(cache.join(".registry-downloads")).expect("create replacement staging root");
        fs::create_dir(&temporary_path).expect("create replacement temporary directory");
        let replacement_sentinel = temporary_path.join("replacement");
        fs::write(&replacement_sentinel, b"replacement")
            .expect("write replacement temporary marker");
        let error = temporary
            .validate_registry_location(Span::default())
            .expect_err("a replaced staging root must fail identity validation");
        assert_eq!(error.code.as_deref(), Some("unsafe_cache_path"));
        drop(temporary);
        assert_eq!(
            fs::read(&replacement_sentinel).expect("read replacement sentinel after guarded drop"),
            b"replacement",
            "guarded cleanup must not remove a path reached through a replacement parent"
        );
        assert_eq!(
            fs::read(
                displaced_staging
                    .join(
                        temporary_path
                            .file_name()
                            .expect("temporary directory name"),
                    )
                    .join("original")
            )
            .expect("read retained original staging marker"),
            b"original"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_quarantine_gc_preserves_unknown_reserved_names() {
        use std::ffi::OsString;

        let root = temp_path("registry-quarantine-gc-reserved-names");
        fs::create_dir_all(root.join("src")).expect("create GC fixture package");
        let _temporary = TemporaryDirectory::new(root.clone());
        fs::write(
            root.join(MANIFEST_FILE),
            "name = \"app\"\nversion = \"1.0.0\"\n",
        )
        .expect("write GC fixture manifest");
        let package = discover_from_dir(&root)
            .expect("discover GC fixture")
            .expect("GC fixture package context");
        let package_cache = package.cache_dir.join(PACKAGE_CACHE_DIR).join("math");
        fs::create_dir_all(&package_cache).expect("create package cache fixture");
        let mut unknown_names = vec![
            OsString::from(REGISTRY_QUARANTINE_PREFIX),
            OsString::from(format!("{REGISTRY_QUARANTINE_PREFIX}unrecognized")),
            OsString::from(format!("{REGISTRY_QUARANTINE_PREFIX}unknown.file-copy")),
        ];
        // Linux filesystems accept arbitrary non-NUL filename bytes. Darwin's
        // filesystem APIs reject this fixture with EILSEQ, so its portable
        // reserved-name behavior remains covered there by the ASCII cases.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStringExt;
            let mut non_utf8 = REGISTRY_QUARANTINE_PREFIX.as_bytes().to_vec();
            non_utf8.push(0xff);
            unknown_names.push(OsString::from_vec(non_utf8));
        }
        // A generated but fresh quarantine must also stay in the dedicated GC
        // branch; its stale-age and bounded-tree checks must not be bypassed.
        unknown_names.push(OsString::from(format!(
            "{REGISTRY_QUARANTINE_PREFIX}{:x}-{}",
            Sha256::digest(b"fresh"),
            operation_nonce()
        )));
        for name in &unknown_names {
            let path = package_cache.join(name);
            fs::create_dir(&path).expect("create retained quarantine fixture");
            fs::write(path.join("sentinel"), b"retain unknown content")
                .expect("write retained quarantine sentinel");
        }
        let unused = package_cache.join("math-0.0.1-fnv64-0000000000000000");
        fs::create_dir(&unused).expect("create ordinary unused cache");
        fs::write(unused.join("unused"), b"unused").expect("write unused cache marker");

        let removed = gc_cache(&package, 64).expect("collect only safe unused cache");
        assert_eq!(
            removed, 1,
            "unknown quarantine names must never fall through to generic GC"
        );
        assert!(
            !unused.exists(),
            "ordinary unused cache must remain collectable"
        );
        for name in &unknown_names {
            assert_eq!(
                fs::read(package_cache.join(name).join("sentinel"))
                    .expect("unknown or fresh quarantine must retain its contents"),
                b"retain unknown content"
            );
        }
    }

    #[test]
    fn registry_quarantine_cleanup_is_plain_tree_and_budget_bounded() {
        let root = temp_path("registry-quarantine-cleanup");
        let cache = root.join("cache");
        let packages = cache.join(PACKAGE_CACHE_DIR);
        let package = packages.join("math");
        fs::create_dir_all(&package).expect("create package cache root");
        let canonical_packages = fs::canonicalize(&packages).expect("resolve packages root");
        let quarantine_name = |seed: &str| {
            format!(
                "{REGISTRY_QUARANTINE_PREFIX}{:x}-{}",
                Sha256::digest(seed.as_bytes()),
                operation_nonce()
            )
        };

        let plain = package.join(quarantine_name("plain"));
        fs::create_dir(&plain).expect("create plain quarantine");
        fs::write(plain.join("entry"), b"plain").expect("write plain quarantine entry");
        let mut plain_budget = CacheGcScanBudget::new(16);
        assert!(cleanup_registry_quarantine_candidate(
            &cache,
            &canonical_packages,
            &package,
            &plain,
            Duration::ZERO,
            &mut plain_budget,
        )
        .expect("clean plain quarantine"));
        assert!(!plain.exists());

        let external = root.join("external-sentinel");
        fs::write(&external, b"outside").expect("write external sentinel");
        let linked = package.join(quarantine_name("linked"));
        fs::create_dir(&linked).expect("create linked quarantine");
        #[cfg(unix)]
        let link_created = std::os::unix::fs::symlink(&external, linked.join("escape")).is_ok();
        #[cfg(windows)]
        let link_created =
            std::os::windows::fs::symlink_file(&external, linked.join("escape")).is_ok();
        if link_created {
            let mut linked_budget = CacheGcScanBudget::new(16);
            assert!(!cleanup_registry_quarantine_candidate(
                &cache,
                &canonical_packages,
                &package,
                &linked,
                Duration::ZERO,
                &mut linked_budget,
            )
            .expect("inspect linked quarantine"));
            assert!(linked.is_dir(), "unsafe quarantine must be retained");
            assert_eq!(
                fs::read(&external).expect("read external sentinel"),
                b"outside"
            );
        }

        let bounded = package.join(quarantine_name("bounded"));
        fs::create_dir(&bounded).expect("create budget quarantine");
        fs::write(bounded.join("one"), b"1").expect("write first budget entry");
        fs::write(bounded.join("two"), b"2").expect("write second budget entry");
        let mut exhausted_budget = CacheGcScanBudget {
            remaining_entries: 1,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert!(!cleanup_registry_quarantine_candidate(
            &cache,
            &canonical_packages,
            &package,
            &bounded,
            Duration::ZERO,
            &mut exhausted_budget,
        )
        .expect("stop bounded quarantine scan"));
        assert!(
            bounded.is_dir(),
            "incompletely scanned quarantine must remain"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_quarantine_identity_failure_never_restores_unknown_content() {
        let root = temp_path("registry-quarantine-identity-change");
        let cache = root.join("cache");
        fs::create_dir_all(&cache).expect("create cache root");
        let checksum = format!("sha256-{}", "b".repeat(64));
        let target = cache
            .join(PACKAGE_CACHE_DIR)
            .join("math")
            .join(registry_cache_key_for_identity("math", "1.0.0", &checksum));
        fs::create_dir_all(&target).expect("create corrupt cache target");
        fs::write(target.join("corrupt"), b"unknown").expect("write corrupt cache marker");
        let plan = RegistryDownloadPlan {
            cache_dir: cache.clone(),
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            url: "https://unreachable.invalid/math.tar.zst".to_string(),
            checksum,
            target_dir: target.clone(),
            temporary_dir: cache
                .join(".registry-downloads")
                .join(format!("math-1.0.0-{}", operation_nonce())),
            action: RegistryCacheAction::DownloadAndReplace,
            policy: RegistryFetchPolicy::default(),
        };
        let cache_root =
            open_registry_cache_directory(&cache, "test registry cache root", Span::default())
                .expect("open cache root");
        let lock_root = ensure_registry_cache_child_directory(
            &cache_root,
            PACKAGE_LOCKS_DIR,
            "test registry package lock root",
            Span::default(),
        )
        .expect("prepare lock root");
        let cache_key = target
            .file_name()
            .and_then(|name| name.to_str())
            .expect("cache key");
        let install_lock = acquire_registry_install_lock(
            &package_operation_lock_path(&cache, "registry", "math", cache_key),
            Span::default(),
        )
        .expect("acquire repair lock");
        let layout = prepare_registry_cache_layout(&plan, cache_root, Span::default())
            .expect("prepare cache layout");
        let expired = quarantine_corrupt_registry_cache(
            &plan,
            &layout,
            Instant::now() - Duration::from_millis(1),
            Span::default(),
        )
        .expect_err("an expired quarantine operation must leave the active target alone");
        assert_eq!(expired.code.as_deref(), Some("registry_resolve_timeout"));
        assert_eq!(
            fs::read(target.join("corrupt")).expect("read unchanged expired quarantine target"),
            b"unknown"
        );
        let mut displaced = None;
        let error = quarantine_corrupt_registry_cache_with_post_rename(
            &plan,
            &layout,
            package_operation_deadline(),
            Span::default(),
            |quarantine| {
                let moved = quarantine.with_file_name(format!(
                    "{}.identity-changed",
                    quarantine
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("quarantine")
                ));
                fs::rename(quarantine, &moved).map_err(|err| {
                    KuError::message(format!("failed to inject quarantine replacement: {err}"))
                })?;
                fs::create_dir(quarantine).map_err(|err| {
                    KuError::message(format!("failed to inject replacement directory: {err}"))
                })?;
                displaced = Some(moved);
                Ok(())
            },
        )
        .expect_err("a changed quarantine identity must fail closed");
        assert_eq!(error.code.as_deref(), Some("unsafe_cache_path"));
        assert!(
            !target.exists(),
            "unknown quarantined content must never be restored to active cache"
        );
        let displaced = displaced.expect("record displaced quarantine");
        assert_eq!(
            fs::read(displaced.join("corrupt")).expect("read displaced unknown content"),
            b"unknown"
        );
        drop(layout);
        drop(install_lock);
        drop(lock_root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pack_staging_cleanup_is_strict_bounded_and_preserves_fresh_entries() {
        let root = temp_path("pack-staging-cleanup");
        let distribution = root.join(PACKAGE_DISTRIBUTION_DIR);
        fs::create_dir_all(&distribution).expect("create package distribution directory");
        let stale_archive = distribution.join(".math-1.0.0-1-2-3.tar.zst.tmp");
        let fresh_archive = distribution.join(".math-1.0.0-4-5-6.tar.zst.tmp");
        let old_identity_archive = distribution.join(".old-package-9.8.7-25-26-27.tar.zst.tmp");
        let stale_verification = distribution.join(".math-1.0.0-7-8-9.verify");
        let malformed = distribution.join(".math-1.0.0-not-a-nonce.tar.zst.tmp");
        let wrong_archive_type = distribution.join(".math-1.0.0-10-11-12.tar.zst.tmp");
        let wrong_verification_type = distribution.join(".math-1.0.0-13-14-15.verify");
        fs::write(&stale_archive, b"partial").expect("write stale archive staging");
        fs::write(&fresh_archive, b"active").expect("write fresh archive staging");
        fs::write(&old_identity_archive, b"old identity")
            .expect("write staging from an older package identity");
        fs::create_dir_all(&stale_verification).expect("create stale verification staging");
        fs::write(stale_verification.join("ku.mod"), b"name = \"math\"\n")
            .expect("write bounded verification staging");
        fs::write(&malformed, b"unrelated").expect("write malformed staging name");
        fs::create_dir_all(&wrong_archive_type).expect("create wrong archive staging type");
        fs::write(&wrong_verification_type, b"wrong type")
            .expect("write wrong verification staging type");
        let symlink_target = distribution.join("symlink-target");
        let symlink_staging = distribution.join(".math-1.0.0-22-23-24.tar.zst.tmp");
        fs::write(&symlink_target, b"target").expect("write symlink target");
        #[cfg(unix)]
        let symlink_created = std::os::unix::fs::symlink(&symlink_target, &symlink_staging).is_ok();
        #[cfg(windows)]
        let symlink_created =
            std::os::windows::fs::symlink_file(&symlink_target, &symlink_staging).is_ok();

        let removed = cleanup_stale_pack_staging(
            &distribution,
            Duration::from_secs(REGISTRY_STAGING_STALE_SECS),
            MAX_PACK_STAGING_SCAN_ENTRIES,
            PackageArchivePolicy::default(),
        )
        .expect("fresh staging scan");
        assert_eq!(removed, 0);
        assert!(fresh_archive.is_file(), "fresh active staging must survive");

        let removed = cleanup_stale_pack_staging(
            &distribution,
            Duration::ZERO,
            MAX_PACK_STAGING_SCAN_ENTRIES,
            PackageArchivePolicy::default(),
        )
        .expect("injected stale staging cleanup");
        assert_eq!(removed, 4);
        assert!(!stale_archive.exists());
        assert!(!fresh_archive.exists());
        assert!(!stale_verification.exists());
        assert!(
            !old_identity_archive.exists(),
            "staging from an older package name/version must not leak forever"
        );
        assert!(malformed.is_file(), "non-generated names must survive");
        assert!(
            wrong_archive_type.is_dir(),
            "wrong entry types must survive"
        );
        assert!(
            wrong_verification_type.is_file(),
            "wrong entry types must survive"
        );
        if symlink_created {
            assert!(
                fs::symlink_metadata(&symlink_staging)
                    .expect("inspect retained staging symlink")
                    .file_type()
                    .is_symlink(),
                "staging cleanup must never delete a symbolic link"
            );
        }

        let budget_verification = distribution.join(".budget-1.0.0-28-29-30.verify");
        fs::create_dir_all(&budget_verification).expect("create budget verification staging");
        fs::write(budget_verification.join("one"), b"1").expect("write first budget entry");
        fs::write(budget_verification.join("two"), b"2").expect("write second budget entry");
        let mut nested_budget =
            PackStagingCleanupBudget::new(1, Duration::from_secs(CACHE_GC_DEADLINE_SECS));
        assert!(
            !verification_staging_tree_is_bounded_and_plain(
                &budget_verification,
                PackageArchivePolicy::default(),
                &mut nested_budget,
            )
            .expect("inspect verification staging with a tiny injected budget"),
            "an incompletely verified directory must not be eligible for deletion"
        );
        assert!(budget_verification.is_dir());

        let first = distribution.join(".math-1.0.0-16-17-18.tar.zst.tmp");
        let second = distribution.join(".math-1.0.0-19-20-21.tar.zst.tmp");
        fs::write(&first, b"one").expect("write first bounded candidate");
        fs::write(&second, b"two").expect("write second bounded candidate");
        let removed = cleanup_stale_pack_staging(
            &distribution,
            Duration::ZERO,
            1,
            PackageArchivePolicy::default(),
        )
        .expect("bounded staging scan");
        assert!(
            removed <= 1,
            "one scanned entry can remove at most one item"
        );
        assert!(
            first.exists() || second.exists(),
            "the scan bound must prevent a full unbounded sweep"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_lock_write_cleans_only_strict_stale_regular_staging_files() {
        let root = temp_path("lock-staging-cleanup");
        fs::create_dir_all(&root).expect("create package lock temp root");
        let lock_path = root.join(LOCK_FILE);
        let stale = root.join("ku.lock.1-2-3.tmp");
        let fresh = root.join("ku.lock.4-5-6.tmp");
        let malformed = root.join("ku.lock.not-a-nonce.tmp");
        let wrong_type = root.join("ku.lock.7-8-9.tmp");
        fs::write(&stale, b"partial").expect("write stale lock staging");
        fs::write(&fresh, b"active").expect("write fresh lock staging");
        fs::write(&malformed, b"unrelated").expect("write malformed lock staging");
        fs::create_dir_all(&wrong_type).expect("create wrong lock staging type");

        assert_eq!(
            cleanup_stale_package_lock_staging(
                &lock_path,
                Duration::from_secs(REGISTRY_STAGING_STALE_SECS),
                MAX_LOCK_STAGING_SCAN_ENTRIES,
            )
            .expect("fresh lock staging scan"),
            0
        );
        assert!(fresh.is_file(), "fresh lock staging must survive");

        write_package_lock_atomically_with_stale_after(&lock_path, b"locked\n", Duration::ZERO)
            .expect("write lock while cleaning stale staging under its io lock");
        assert_eq!(
            fs::read(&lock_path).expect("read installed package lock"),
            b"locked\n"
        );
        assert!(!stale.exists());
        assert!(!fresh.exists());
        assert!(malformed.is_file(), "non-generated names must survive");
        assert!(wrong_type.is_dir(), "directories must never be removed");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pack_compressed_limit_stops_the_writer_and_raii_removes_staging() {
        let root = temp_path("pack-compressed-limit");
        let package_root = root.join("math");
        fs::create_dir_all(package_root.join("src")).expect("create package source");
        fs::write(
            package_root.join(MANIFEST_FILE),
            b"name = \"math\"\nversion = \"1.0.0\"\n",
        )
        .expect("write package manifest");
        fs::write(
            package_root.join("src").join("math.ku"),
            b"fn Value(): int { return 42 }\n",
        )
        .expect("write package source");
        let package = discover_from_dir(&package_root)
            .expect("discover package")
            .expect("package context");
        let policy = PackageArchivePolicy {
            max_compressed_bytes: 1,
            ..PackageArchivePolicy::default()
        };
        let err = pack_package_with_policy(&package, policy)
            .expect_err("the compressed writer must stop before crossing its byte cap");
        assert_eq!(err.code.as_deref(), Some("package_archive_limit"));
        let distribution = package_root.join(PACKAGE_DISTRIBUTION_DIR);
        for entry in fs::read_dir(&distribution).expect("read distribution after failed pack") {
            let file_name = entry
                .expect("read distribution entry")
                .file_name()
                .to_string_lossy()
                .into_owned();
            assert!(
                !file_name.ends_with(".tar.zst.tmp") && !file_name.ends_with(".verify"),
                "RAII must remove failed pack staging, found {file_name}"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_names_reject_windows_reserved_device_basenames_on_every_host() {
        for name in [
            "con", "prn", "aux", "nul", "com1", "com9", "lpt1", "lpt9", "CON", "con.txt", "nul. ",
            "LPT9.log",
        ] {
            let err = validate_package_name(name, Span::default())
                .expect_err("Windows reserved device basename must be rejected on every host");
            assert_eq!(err.code.as_deref(), Some("invalid_name"), "{name}");
        }
        for name in ["com0", "com10", "lpt0", "lpt10", "con-tools"] {
            validate_package_name(name, Span::default())
                .unwrap_or_else(|err| panic!("portable package name '{name}' failed: {err:?}"));
        }
        for component in ["CON.txt", "aux...", "Lpt1.json"] {
            let err = validate_portable_package_component(component, Span::default())
                .expect_err("archive components use the same reserved basename semantics");
            assert_eq!(err.code.as_deref(), Some("unsafe_archive_path"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_archive_final_path_check_is_exact_and_component_bounded() {
        let root = r"\\?\C:\packages\Math";
        assert!(windows_path_is_strict_child(
            root,
            r"\\?\C:\packages\Math\src\main.ku"
        ));
        assert!(!windows_path_is_strict_child(
            root,
            r"\\?\C:\packages\Math-copy\src\main.ku"
        ));
        assert!(!windows_path_is_strict_child(
            root,
            r"\\?\C:\packages\math\src\main.ku"
        ));
        assert!(!windows_path_is_strict_child(root, root));
    }

    #[test]
    fn package_pack_rejects_identity_change_between_enumeration_and_read() {
        let root = temp_path("package-pack-identity-race");
        let package_root = root.join("math");
        fs::create_dir_all(package_root.join("src")).expect("create package src");
        fs::write(
            package_root.join(MANIFEST_FILE),
            b"name = \"math\"\nversion = \"1.0.0\"\n",
        )
        .expect("write manifest");
        let source = package_root.join("src").join("math.ku");
        fs::write(&source, b"fn Value(): int { return 1 }\n").expect("write source");
        let package = discover_from_dir(&package_root)
            .expect("discover package")
            .expect("package context");
        let policy = PackageArchivePolicy::default();
        let collected = collect_package_archive_entries(&package, policy, Span::default())
            .expect("enumerate package source");

        let original = package_root.join("src").join("math-original.ku");
        fs::rename(&source, &original).expect("move enumerated source inode");
        fs::write(&source, b"fn Value(): int { return 2 }\n")
            .expect("replace source with same-sized different file");
        let archive = root.join("identity-race.tar.zst");
        let err = write_deterministic_package_archive(
            &archive,
            "math",
            "1.0.0",
            &collected,
            policy.max_compressed_bytes,
            Span::default(),
        )
        .expect_err("replaced source identity must fail closed");
        assert_eq!(err.code.as_deref(), Some("package_source_changed"));
        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_pack_does_not_follow_replaced_source_directory() {
        let root = temp_path("package-pack-directory-race");
        let package_root = root.join("math");
        let outside = root.join("outside");
        fs::create_dir_all(package_root.join("src")).expect("create package src");
        fs::create_dir_all(&outside).expect("create outside directory");
        fs::write(
            package_root.join(MANIFEST_FILE),
            b"name = \"math\"\nversion = \"1.0.0\"\n",
        )
        .expect("write manifest");
        fs::write(
            package_root.join("src").join("math.ku"),
            b"fn Value(): int { return 1 }\n",
        )
        .expect("write source");
        fs::write(
            outside.join("math.ku"),
            b"SECRET_OUTSIDE_PACKAGE_ROOT_4J8W\n",
        )
        .expect("write outside sentinel");
        let package = discover_from_dir(&package_root)
            .expect("discover package")
            .expect("package context");
        let policy = PackageArchivePolicy::default();
        let collected = collect_package_archive_entries(&package, policy, Span::default())
            .expect("enumerate package source");

        let original = package_root.join("src-original");
        fs::rename(package_root.join("src"), &original).expect("move enumerated source directory");
        let linked = create_test_directory_symlink(&outside, &package_root.join("src")).is_ok();
        if linked {
            let archive = root.join("directory-race.tar.zst");
            let err = write_deterministic_package_archive(
                &archive,
                "math",
                "1.0.0",
                &collected,
                policy.max_compressed_bytes,
                Span::default(),
            )
            .expect_err("a replacement link or reparse point must not be followed");
            assert!(
                matches!(
                    err.code.as_deref(),
                    Some("package_source_changed" | "unsafe_package_path")
                ),
                "unexpected failure: {err:?}"
            );
            let _ = fs::remove_file(package_root.join("src"));
            let _ = fs::remove_dir(package_root.join("src"));
            let _ = fs::remove_file(archive);
        } else {
            eprintln!("skip directory-link race subcase: host denied symlink creation");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn package_pack_rejects_replaced_fifo_without_blocking() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let root = temp_path("package-pack-fifo-race");
        let package_root = root.join("math");
        fs::create_dir_all(package_root.join("src")).expect("create package src");
        fs::write(
            package_root.join(MANIFEST_FILE),
            b"name = \"math\"\nversion = \"1.0.0\"\n",
        )
        .expect("write manifest");
        let source = package_root.join("src").join("math.ku");
        fs::write(&source, b"fn Value(): int { return 1 }\n").expect("write source");
        let package = discover_from_dir(&package_root)
            .expect("discover package")
            .expect("package context");
        let policy = PackageArchivePolicy::default();
        let collected = collect_package_archive_entries(&package, policy, Span::default())
            .expect("enumerate package source");

        fs::rename(&source, package_root.join("src").join("math-original.ku"))
            .expect("move enumerated source");
        let fifo = CString::new(source.as_os_str().as_bytes()).expect("FIFO path has no NUL");
        // SAFETY: `fifo` is a valid NUL-terminated path and the mode contains
        // only ordinary permission bits.
        let result = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        assert_eq!(result, 0, "create FIFO: {}", io::Error::last_os_error());
        let archive = root.join("fifo-race.tar.zst");
        let started = Instant::now();
        let err = write_deterministic_package_archive(
            &archive,
            "math",
            "1.0.0",
            &collected,
            policy.max_compressed_bytes,
            Span::default(),
        )
        .expect_err("replacement FIFO must fail closed without waiting for a writer");
        assert_eq!(err.code.as_deref(), Some("package_source_changed"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "FIFO rejection unexpectedly blocked"
        );
        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    fn create_test_directory_symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_test_directory_symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[cfg(not(any(unix, windows)))]
    fn create_test_directory_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory links are not supported on this host",
        ))
    }

    #[test]
    fn package_pack_is_deterministic_allowlisted_and_round_trips() {
        let root = temp_path("package-pack");
        let package_root = root.join("math");
        fs::create_dir_all(package_root.join("src")).expect("create package src");
        fs::create_dir_all(package_root.join(".git")).expect("create excluded vcs dir");
        fs::create_dir_all(package_root.join("src").join(".git"))
            .expect("create nested excluded vcs dir");
        fs::create_dir_all(package_root.join("docs").join("cache"))
            .expect("create custom cache dir");
        fs::create_dir_all(package_root.join("examples")).expect("create examples dir");
        fs::create_dir_all(package_root.join("tests").join(".ku"))
            .expect("create nested cache dir");
        fs::write(
            package_root.join(MANIFEST_FILE),
            b"name = \"math\"\nversion = \"1.2.3\"\nroot = \"src\"\ncache = \"docs/cache\"\n",
        )
        .expect("write manifest");
        fs::write(
            package_root.join("src").join("math.ku"),
            b"fn Add(a: int, b: int): int { return a + b }\n",
        )
        .expect("write source");
        fs::write(package_root.join(".env"), b"SECRET=do-not-publish")
            .expect("write excluded secret");
        fs::write(package_root.join(".git").join("config"), b"private")
            .expect("write excluded vcs data");
        fs::write(package_root.join("src").join(".env"), b"SOURCE_SECRET=1")
            .expect("write nested source secret");
        fs::write(
            package_root.join("src").join(".git").join("config"),
            b"nested-private",
        )
        .expect("write nested vcs data");
        fs::write(package_root.join("docs").join("ku.lock"), b"secret-lock")
            .expect("write nested lock");
        fs::write(
            package_root
                .join("docs")
                .join("cache")
                .join("dependency.ku"),
            b"private cache",
        )
        .expect("write custom cache data");
        fs::write(
            package_root.join("examples").join(".env.production"),
            b"TOKEN=private",
        )
        .expect("write nested environment secret");
        fs::write(
            package_root.join("tests").join(".ku").join("cache.bin"),
            b"private cache",
        )
        .expect("write nested test cache");
        let package = discover_from_dir(&package_root)
            .expect("discover package")
            .expect("package context");

        let first = pack_package(&package).expect("first deterministic pack");
        let first_bytes = fs::read(&first.path).expect("read first artifact");
        let second = pack_package(&package).expect("second deterministic pack");
        assert_eq!(first.path, second.path);
        assert_eq!(first.checksum, second.checksum);
        assert_eq!(
            first_bytes,
            fs::read(&second.path).expect("read second artifact")
        );
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "pack".to_string(),
            package_root.to_string_lossy().to_string(),
        ])
        .expect("CLI package pack");

        let unpacked = root.join("unpacked");
        unpack_package_archive(
            &first.path,
            &unpacked,
            PackageArchivePolicy::default(),
            Span::default(),
        )
        .expect("round-trip artifact");
        assert!(unpacked.join("src").join("math.ku").is_file());
        assert!(!unpacked.join(".env").exists());
        assert!(!unpacked.join(".git").exists());
        assert!(!unpacked.join(".ku").exists());
        assert!(!unpacked.join("src").join(".env").exists());
        assert!(!unpacked.join("src").join(".git").exists());
        assert!(!unpacked.join("docs").join("ku.lock").exists());
        assert!(!unpacked.join("docs").join("cache").exists());
        assert!(!unpacked.join("examples").join(".env.production").exists());
        assert!(!unpacked.join("tests").join(".ku").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_pack_rejects_executable_and_install_script_payloads() {
        let root = temp_path("package-pack-script");
        let package_root = root.join("math");
        fs::create_dir_all(package_root.join("src")).expect("create package src");
        fs::create_dir_all(package_root.join("examples")).expect("create examples");
        fs::write(
            package_root.join(MANIFEST_FILE),
            b"name = \"math\"\nversion = \"1.0.0\"\n",
        )
        .expect("write manifest");
        fs::write(
            package_root.join("src").join("math.ku"),
            b"fn Value(): int { return 1 }",
        )
        .expect("write source");
        fs::write(
            package_root.join("examples").join("install.ps1"),
            b"Write-Host unsafe",
        )
        .expect("write script");
        let package = discover_from_dir(&package_root)
            .expect("discover package")
            .expect("package context");
        let err = pack_package(&package).expect_err("install script must be rejected");
        assert_eq!(err.code.as_deref(), Some("unsupported_package_entry"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn https_registry_publish_resolve_check_run_and_build_is_a_signed_closed_loop() {
        const TOKEN: &str = "KU_TEST_TOKEN_SENTINEL_7H4L2";
        const REJECTED_TOKEN: &str = "KU_TEST_TOKEN_REJECTED_9P3X6";

        let signing_key = SigningKey::from_bytes(&[37u8; 32]);
        let public_key = signing_key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let (registry, agent) = TestHttpsRegistry::start(signing_key, TOKEN);
        let agent_guard = TestRegistryHttpAgentGuard::install(registry.base_url.clone(), agent);

        let root = temp_path("registry-https-e2e");
        let _ = fs::remove_dir_all(&root);
        let author_dir = root.join("author");
        let consumer_dir = root.join("consumer");
        fs::create_dir_all(author_dir.join("src")).expect("create author source directory");
        fs::create_dir_all(consumer_dir.join("src")).expect("create consumer source directory");
        fs::write(
            author_dir.join(MANIFEST_FILE),
            format!(
                "name = \"greeter\"\nversion = \"1.0.0\"\nroot = \"src\"\nregistry.url = \"{}\"\nregistry.public_key = \"ed25519-{public_key}\"\n",
                registry.base_url
            ),
        )
        .expect("write author manifest");
        fs::write(
            author_dir.join("src").join("greeter.ku"),
            b"fn Value(): int { return 42 }\n",
        )
        .expect("write published source");
        let author = discover_from_dir(&author_dir)
            .expect("discover author package")
            .expect("author package");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "pack".to_string(),
            author_dir.to_string_lossy().to_string(),
        ])
        .expect("pack author through the public CLI");
        let packed = pack_package(&author).expect("pack author package");
        assert!(packed.path.is_file());

        let token_environment = TestEnvironmentVariableGuard::unset(REGISTRY_TOKEN_ENV);
        let missing_token = crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "publish".to_string(),
            author_dir.to_string_lossy().to_string(),
        ])
        .expect_err("CLI publish without KU_REGISTRY_TOKEN must fail");
        assert!(missing_token.to_string().contains(REGISTRY_TOKEN_ENV));
        assert!(!missing_token.to_string().contains(TOKEN));
        assert!(!missing_token.to_string().contains(REJECTED_TOKEN));

        token_environment.set(REJECTED_TOKEN);
        let rejected = crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "publish".to_string(),
            author_dir.to_string_lossy().to_string(),
        ])
        .expect_err("registry must reject a different bearer token");
        assert_eq!(
            rejected.code.as_deref(),
            Some("registry_publish_unauthorized")
        );
        assert!(!rejected.to_string().contains(REJECTED_TOKEN));
        assert!(!rejected.to_string().contains(TOKEN));

        token_environment.set(TOKEN);
        let clients_before_publish = agent_guard.matched_client_count();
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "publish".to_string(),
            author_dir.to_string_lossy().to_string(),
        ])
        .unwrap_or_else(|err| {
            panic!(
                "publish over verified HTTPS failed: {err:?}; server state: {:?}",
                registry
                    .state
                    .lock()
                    .expect("test registry state mutex poisoned")
            )
        });
        assert_eq!(
            agent_guard.matched_client_count(),
            clients_before_publish + 1,
            "one publish operation must reuse one HTTP client for PUT and signed-index verification"
        );
        let publish_stdout = crate::cli::package_publish_success_message(&PackagePublishReceipt {
            name: "greeter".to_string(),
            version: "1.0.0".to_string(),
            checksum: packed.checksum.clone(),
            registry: registry.base_url.clone(),
        });
        assert!(!publish_stdout.contains(TOKEN));
        assert!(!publish_stdout.contains(REJECTED_TOKEN));

        fs::write(
            consumer_dir.join(MANIFEST_FILE),
            format!(
                "name = \"consumer\"\nversion = \"0.1.0\"\nroot = \"src\"\nregistry.url = \"{}\"\nregistry.public_key = \"ed25519-{public_key}\"\ndep.greeter = \"^1.0.0\"\n",
                registry.base_url
            ),
        )
        .expect("write consumer manifest");
        let main_path = consumer_dir.join("src").join("main.ku");
        fs::write(
            &main_path,
            b"import { Value } from \"@greeter/greeter\"\nfn main() { println(Value()) }\n",
        )
        .expect("write consumer source");

        let clients_before_resolve = agent_guard.matched_client_count();
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "resolve".to_string(),
            consumer_dir.to_string_lossy().to_string(),
        ])
        .expect("resolve consumer from signed HTTPS registry");
        assert_eq!(
            agent_guard.matched_client_count(),
            clients_before_resolve + 1,
            "one resolve operation must reuse one HTTP client for signature, index retries, and artifact download"
        );
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "check".to_string(),
            main_path.to_string_lossy().to_string(),
        ])
        .expect("check consumer with resolved package");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "run".to_string(),
            main_path.to_string_lossy().to_string(),
        ])
        .expect("run consumer with resolved package");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "build".to_string(),
            "--native".to_string(),
            main_path.to_string_lossy().to_string(),
        ])
        .expect("build consumer with resolved package");
        let native_c = main_path.with_extension("c");
        assert!(native_c.is_file(), "native package build did not emit C");
        assert!(
            fs::read_to_string(&native_c)
                .expect("read native package build")
                .contains("Value"),
            "native package build did not include the imported package graph"
        );

        let lock = fs::read_to_string(consumer_dir.join(LOCK_FILE)).expect("read consumer lock");
        assert!(lock.contains("name = \"greeter\""));
        assert!(!lock.contains(TOKEN));
        assert!(!lock.contains(REJECTED_TOKEN));
        assert_tree_does_not_contain(&root, TOKEN.as_bytes());
        assert_tree_does_not_contain(&root, REJECTED_TOKEN.as_bytes());

        let state_handle = Arc::clone(&registry.state);
        let online_request_counts = {
            let state = state_handle
                .lock()
                .expect("test registry state mutex poisoned");
            (
                state.signature_reads,
                state.index_reads,
                state.artifact_reads,
                state.successful_uploads,
                state.rejected_uploads,
            )
        };
        drop(registry);

        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "resolve".to_string(),
            consumer_dir.to_string_lossy().to_string(),
            "--offline".to_string(),
        ])
        .expect("real downloaded lock and cache must resolve after the registry stops");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "check".to_string(),
            main_path.to_string_lossy().to_string(),
        ])
        .expect("check must reuse the real downloaded lock and cache without a registry");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "run".to_string(),
            main_path.to_string_lossy().to_string(),
        ])
        .expect("run must reuse the real downloaded lock and cache without a registry");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "build".to_string(),
            "--native".to_string(),
            main_path.to_string_lossy().to_string(),
        ])
        .expect("native build must reuse the real downloaded lock and cache without a registry");

        let state = state_handle
            .lock()
            .expect("test registry state mutex poisoned");
        assert_eq!(
            (
                state.signature_reads,
                state.index_reads,
                state.artifact_reads,
                state.successful_uploads,
                state.rejected_uploads,
            ),
            online_request_counts,
            "offline and lock/cache-backed commands must not issue registry requests"
        );
        assert_eq!(state.successful_uploads, 1);
        assert_eq!(state.rejected_uploads, 1);
        assert_eq!(
            state.artifact.as_deref().map(sha256_checksum),
            Some(packed.checksum.clone())
        );
        assert!(
            state.signature_reads >= 3 && state.index_reads >= 3,
            "publish must retry a mixed signature/index generation, then resolve a fresh pair: {state:?}"
        );
        assert_eq!(state.artifact_reads, 1);
        assert!(state.protocol_errors.is_empty(), "{state:?}");
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn locked_transitive_registry_graph_imports_offline_and_detects_tampering() {
        let root = temp_path("registry-offline-e2e");
        let _ = fs::remove_dir_all(&root);
        let core_dir = root.join("core");
        let util_dir = root.join("util");
        let app_dir = root.join("app");
        for directory in [&core_dir, &util_dir, &app_dir] {
            fs::create_dir_all(directory.join("src")).expect("create package src");
        }
        fs::create_dir_all(util_dir.join("src").join("internal"))
            .expect("create util internal source");
        fs::write(
            core_dir.join(MANIFEST_FILE),
            b"name = \"core\"\nversion = \"1.0.0\"\nroot = \"src\"\n",
        )
        .expect("write core manifest");
        fs::write(
            core_dir.join("src").join("core.ku"),
            b"fn Base(): int { return 41 }\n",
        )
        .expect("write core source");
        fs::write(
            util_dir.join(MANIFEST_FILE),
            b"name = \"util\"\nversion = \"1.0.0\"\nroot = \"src\"\ndep.core = \"^1.0.0\"\n",
        )
        .expect("write util manifest");
        fs::write(
            util_dir.join("src").join("internal").join("base.ku"),
            b"import { Base } from \"@core/core\"\nfn InternalBase(): int { return Base() }\n",
        )
        .expect("write util internal source");
        fs::write(
            util_dir.join("src").join("layer.ku"),
            b"import { InternalBase } from \"internal/base\"\nfn LayerValue(): int { return InternalBase() + 1 }\n",
        )
        .expect("write util root-relative source");
        fs::write(
            util_dir.join("src").join("entry.ku"),
            b"import { LayerValue } from \"./layer\"\nfn Value(): int { return LayerValue() }\n",
        )
        .expect("write util public entry source");
        let core_package = discover_from_dir(&core_dir)
            .expect("discover core")
            .expect("core package");
        let util_package = discover_from_dir(&util_dir)
            .expect("discover util")
            .expect("util package");
        let core_artifact = pack_package(&core_package).expect("pack core");
        let util_artifact = pack_package(&util_package).expect("pack util");

        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let public_key = signing_key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        fs::write(
            app_dir.join(MANIFEST_FILE),
            format!(
                "name = \"app\"\nversion = \"0.1.0\"\nroot = \"src\"\nregistry.url = \"https://registry.example/v1/\"\nregistry.public_key = \"ed25519-{public_key}\"\ndep.util = \"^1.0.0\"\n"
            ),
        )
        .expect("write app manifest");
        let main_path = app_dir.join("src").join("main.ku");
        fs::write(
            &main_path,
            b"import { Value } from \"@util/entry\"\nfn main() { if (Value() != 42) { panic(\"bad registry import\") } }\n",
        )
        .expect("write app source");
        let mut app_package = discover_from_dir(&app_dir)
            .expect("discover app")
            .expect("app package");
        let core_dependency = PackageDependency {
            name: "core".to_string(),
            version: "^1.0.0".to_string(),
            source: None,
            checksum: None,
        };
        let (core_manifest, core_root, core_cache_key) =
            seed_registry_artifact(&app_package.cache_dir, &core_artifact, Vec::new());
        let (util_manifest, util_root, util_cache_key) = seed_registry_artifact(
            &app_package.cache_dir,
            &util_artifact,
            vec![core_dependency],
        );
        app_package.resolved_registry_dependencies = vec![
            ResolvedPackageDependency {
                name: "core".to_string(),
                requirement: "^1.0.0".to_string(),
                version: core_manifest.version.clone(),
                registry: "https://registry.example/v1/".to_string(),
                url: core_manifest.source.clone(),
                checksum: core_manifest.checksum.clone(),
                cache_key: core_cache_key,
                package_root: core_root.clone(),
            },
            ResolvedPackageDependency {
                name: "util".to_string(),
                requirement: "^1.0.0".to_string(),
                version: util_manifest.version.clone(),
                registry: "https://registry.example/v1/".to_string(),
                url: util_manifest.source.clone(),
                checksum: util_manifest.checksum.clone(),
                cache_key: util_cache_key,
                package_root: util_root,
            },
        ];
        write_lock(&app_package).expect("write exact transitive lock");
        app_package.resolved_registry_dependencies.clear();
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "resolve".to_string(),
            app_dir.to_string_lossy().to_string(),
            "--offline".to_string(),
        ])
        .expect("CLI offline resolve");
        resolve_remote_dependencies_with_mode(&mut app_package, DependencyResolveMode::Offline)
            .expect("resolve complete graph from lock and verified cache");
        assert_eq!(app_package.resolved_registry_dependencies.len(), 2);
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "check".to_string(),
            main_path.to_string_lossy().to_string(),
        ])
        .expect("check transitive registry import");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "run".to_string(),
            main_path.to_string_lossy().to_string(),
        ])
        .expect("run transitive registry import");

        let undeclared_path = app_dir.join("src").join("undeclared.ku");
        fs::write(
            &undeclared_path,
            b"import { Base } from \"@core/core\"\nfn main() { print(Base()) }\n",
        )
        .expect("write undeclared transitive import");
        let err = crate::cli::run_cli(vec![
            "ku".to_string(),
            "check".to_string(),
            undeclared_path.to_string_lossy().to_string(),
        ])
        .expect_err("consumer must not import an undeclared transitive dependency");
        assert!(
            err.to_string()
                .contains("package 'app' does not declare dependency 'core'"),
            "unexpected error: {err:?}"
        );

        fs::write(
            core_root.join("src").join("core.ku"),
            b"fn Base(): int { return 999 }\n",
        )
        .expect("tamper transitive cache");
        let mut tampered_package = discover_from_dir(&app_dir)
            .expect("rediscover app")
            .expect("app package");
        let err = resolve_remote_dependencies_with_mode(
            &mut tampered_package,
            DependencyResolveMode::Offline,
        )
        .expect_err("tampered transitive cache must fail closed");
        assert_eq!(
            err.code.as_deref(),
            Some("offline_cache_miss"),
            "unexpected error: {err:?}"
        );
        let corrupted_install = core_root
            .parent()
            .expect("core cache install root")
            .to_path_buf();
        let removed = gc_cache(&tampered_package, 64)
            .expect("cache GC must be able to remove a corrupt locked entry");
        assert!(removed >= 1);
        assert!(!corrupted_install.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_index_dependencies_drive_bounded_backtracking_solver() {
        let dependency = |name: &str, version: &str| PackageDependency {
            name: name.to_string(),
            version: version.to_string(),
            source: None,
            checksum: None,
        };
        let manifest =
            |name: &str, version: &str, dependencies: Vec<PackageDependency>| RegistryManifest {
                name: name.to_string(),
                version: version.to_string(),
                source: format!("https://registry.example/packages/{name}-{version}.tar.zst"),
                checksum: format!("sha256-{}", "a".repeat(64)),
                dependencies,
            };
        let registry = PackageRegistry {
            url: "https://registry.example/v1/".to_string(),
            public_key: format!("ed25519-{}", "0".repeat(64)),
        };
        let mut solver = RegistryGraphSolver::new(&registry);
        solver
            .insert_test_index(RegistryIndex {
                name: "a".to_string(),
                versions: vec![
                    manifest("a", "1.2.0", vec![dependency("c", "2.0.0")]),
                    manifest("a", "1.1.0", vec![dependency("c", "1.0.0")]),
                ],
            })
            .expect("seed a index");
        solver
            .insert_test_index(RegistryIndex {
                name: "b".to_string(),
                versions: vec![manifest("b", "1.0.0", vec![dependency("c", "1.0.0")])],
            })
            .expect("seed b index");
        solver
            .insert_test_index(RegistryIndex {
                name: "c".to_string(),
                versions: vec![
                    manifest("c", "2.0.0", Vec::new()),
                    manifest("c", "1.0.0", Vec::new()),
                ],
            })
            .expect("seed c index");

        let solved = solver
            .solve(&[dependency("a", "^1.0.0"), dependency("b", "^1.0.0")])
            .expect("solver should backtrack from a 1.2.0 to 1.1.0");
        assert_eq!(solved["a"].version, "1.1.0");
        assert_eq!(solved["b"].version, "1.0.0");
        assert_eq!(solved["c"].version, "1.0.0");
        assert!(solver.steps <= MAX_REGISTRY_SOLVER_STEPS);
    }

    #[test]
    fn registry_index_parses_signed_dependency_metadata() {
        let index = parse_registry_index(
            r#"
name = "math"
[[version]]
version = "1.2.3"
url = "math-1.2.3.tar.zst"
checksum = "sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
dep.core = "^2.0.0"
"#,
            "https://registry.example/v1/packages/math/index.toml",
            Span::default(),
        )
        .expect("registry dependency metadata");
        assert_eq!(index.versions[0].dependencies.len(), 1);
        assert_eq!(index.versions[0].dependencies[0].name, "core");
        assert_eq!(index.versions[0].dependencies[0].version, "^2.0.0");
    }

    #[test]
    fn operating_system_lock_blocks_competitors_and_releases_on_drop() {
        let root = temp_path("lock-owner");
        fs::create_dir_all(&root).expect("create lock root");
        let path = root.join("install.lock");
        let guard = acquire_registry_install_lock(&path, Span::default()).expect("acquire lock");
        assert!(
            try_acquire_registry_install_lock(&path)
                .expect("probe held lock")
                .is_none(),
            "a competing handle must not acquire the same OS lock"
        );
        drop(guard);
        let next = try_acquire_registry_install_lock(&path)
            .expect("probe released lock")
            .expect("OS lock must release when its guard drops");
        drop(next);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_download_slots_cap_parallel_work_and_wait_with_deadline() {
        let root = temp_path("registry-slots");
        let checksum = format!("sha256-{}", "a".repeat(64));
        let target = root
            .join("cache")
            .join(PACKAGE_CACHE_DIR)
            .join("math")
            .join(registry_cache_key_for_identity("math", "1.0.0", &checksum));
        fs::create_dir_all(target.parent().expect("target parent")).expect("create cache path");
        let slots = root.join("cache").join(".registry-slots");
        fs::create_dir_all(&slots).expect("create slot directory");
        let mut guards = Vec::new();
        for slot in 0..MAX_REGISTRY_PARALLELISM {
            guards.push(
                try_acquire_registry_install_lock(&slots.join(format!("slot-{slot}.lock")))
                    .expect("acquire slot")
                    .expect("slot must be free"),
            );
        }
        let plan = RegistryDownloadPlan {
            cache_dir: root.join("cache"),
            name: "math".to_string(),
            version: "1.0.0".to_string(),
            url: "https://registry.example/math.tar.zst".to_string(),
            checksum,
            target_dir: target,
            temporary_dir: root
                .join("cache")
                .join(".registry-downloads")
                .join(format!("math-1.0.0-{}", operation_nonce())),
            action: RegistryCacheAction::DownloadAndReplace,
            policy: RegistryFetchPolicy::default(),
        };
        let err = match acquire_registry_download_slot(
            &plan,
            Span::default(),
            Instant::now() + Duration::from_millis(20),
        ) {
            Ok(_) => panic!("a ninth download must stop at the shared deadline"),
            Err(err) => err,
        };
        assert_eq!(err.code.as_deref(), Some("registry_resolve_timeout"));
        drop(guards);
        acquire_registry_download_slot(
            &plan,
            Span::default(),
            Instant::now() + Duration::from_secs(1),
        )
        .expect("a slot must become available after guards drop");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn publish_plan_token_and_status_contract_are_bounded() {
        let package = PackageContext {
            manifest_path: PathBuf::from("ku.mod"),
            package_dir: PathBuf::from("."),
            import_root: PathBuf::from("src"),
            cache_dir: PathBuf::from(".ku/cache"),
            lock_path: PathBuf::from("ku.lock"),
            manifest: KuMod {
                name: "math".to_string(),
                version: Some("1.2.3".to_string()),
                root: Some("src".to_string()),
                main: None,
                out: None,
                cache: None,
                template: None,
                package_type: Some("lib".to_string()),
                registry: Some(PackageRegistry {
                    url: "https://registry.example/v1/".to_string(),
                    public_key: format!("ed25519-{}", "0".repeat(64)),
                }),
                dependencies: Vec::new(),
            },
            resolved_file_dependencies: Vec::new(),
            resolved_registry_dependencies: Vec::new(),
        };
        let artifact = PackageArtifact {
            path: PathBuf::from("math.tar.zst"),
            name: "math".to_string(),
            version: "1.2.3".to_string(),
            checksum: format!("sha256-{}", "a".repeat(64)),
            size: 123,
        };
        let plan = plan_package_publish(&package, &artifact).expect("publish plan");
        assert_eq!(plan.url, "https://registry.example/v1/packages/math/1.2.3");
        assert_eq!(plan.content_length, 123);
        assert!(plan.idempotency_key.contains(&artifact.checksum));

        validate_publish_token("valid-secret", Span::default()).expect("valid token");
        let invalid_token = "do-not-leak\n";
        let err = validate_publish_token(invalid_token, Span::default())
            .expect_err("control characters must be rejected");
        assert_eq!(err.code.as_deref(), Some("invalid_publish_token"));
        assert!(!err.to_string().contains("do-not-leak"));
        let unicode_secret = "sentinel_密钥";
        let err = validate_publish_token(unicode_secret, Span::default())
            .expect_err("non-ASCII bearer tokens must be rejected before header construction");
        assert_eq!(err.code.as_deref(), Some("invalid_publish_token"));
        assert!(!err.to_string().contains(unicode_secret));

        for status in [200, 201, 204] {
            validate_publish_http_status(status, &artifact).expect("success status");
        }
        for (status, code) in [
            (401, "registry_publish_unauthorized"),
            (403, "registry_publish_unauthorized"),
            (409, "package_version_conflict"),
            (429, "registry_publish_status"),
            (500, "registry_publish_status"),
        ] {
            let err = validate_publish_http_status(status, &artifact)
                .expect_err("failure status must be structured");
            assert_eq!(err.code.as_deref(), Some(code));
        }
        let committed = RegistryManifest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            source: "https://registry.example/v1/artifacts/math.tar.zst".to_string(),
            checksum: artifact.checksum.clone(),
            dependencies: Vec::new(),
        };
        verify_published_index(
            &RegistryIndex {
                name: artifact.name.clone(),
                versions: vec![committed.clone()],
            },
            &artifact,
            &[],
        )
        .expect("matching signed index must confirm publish");
        let err = verify_published_index(
            &RegistryIndex {
                name: "other".to_string(),
                versions: vec![committed],
            },
            &artifact,
            &[],
        )
        .expect_err("mismatched signed index name must fail post-publish verification");
        assert_eq!(err.code.as_deref(), Some("publish_verification_failed"));
    }

    #[test]
    fn file_snapshot_is_allowlisted_content_addressed_and_reader_safe_during_refresh() {
        let root = temp_path("file-snapshot-race");
        let _ = fs::remove_dir_all(&root);
        let source = root.join("util");
        let app = root.join("app");
        fs::create_dir_all(source.join("src")).expect("create file package source");
        fs::create_dir_all(app.join("src")).expect("create consumer source");
        fs::write(
            source.join(MANIFEST_FILE),
            "name = \"util\"\nversion = \"1.0.0\"\n",
        )
        .expect("write file package manifest");
        for index in 0..32 {
            fs::write(
                source.join("src").join(format!("m{index:02}.ku")),
                format!("fn V{index}(): int {{ return 1 }}\n"),
            )
            .expect("write file package module");
        }
        for excluded in [".git", ".ku", "node_modules"] {
            fs::create_dir_all(source.join(excluded)).expect("create excluded directory");
            fs::write(source.join(excluded).join("secret"), b"SECRET_ONE")
                .expect("write excluded secret");
        }
        fs::write(source.join(".env"), b"TOKEN=SECRET_ONE").expect("write excluded env");
        fs::write(source.join("README.md"), b"not runtime input").expect("write excluded readme");
        let checksum_before = package_source_checksum(&source).expect("checksum runtime snapshot");
        fs::write(source.join(".env"), b"TOKEN=SECRET_TWO").expect("change excluded env");
        fs::write(source.join(".git").join("secret"), b"SECRET_TWO")
            .expect("change excluded metadata");
        fs::write(source.join("README.md"), b"changed documentation")
            .expect("change excluded readme");
        assert_eq!(
            package_source_checksum(&source).expect("checksum after excluded changes"),
            checksum_before,
            "non-runtime files must not perturb the file package identity"
        );
        fs::write(
            app.join(MANIFEST_FILE),
            format!(
                "name = \"app\"\nversion = \"0.1.0\"\ndep.util = \"1.0.0\"\ndep.util.source = \"file://{}\"\n",
                source.to_string_lossy().replace('\\', "/")
            ),
        )
        .expect("write consumer manifest");
        fs::write(app.join("src").join("main.ku"), b"fn main() {}\n")
            .expect("write consumer source");

        let mut first = discover_from_dir(&app)
            .expect("discover first consumer")
            .expect("first consumer package");
        resolve_remote_dependencies(&mut first).expect("resolve first immutable snapshot");
        let first_dependency = first.resolved_file_dependencies[0].clone();
        assert_eq!(first_dependency.checksum, checksum_before);
        assert!(!first_dependency.package_root.join(".env").exists());
        assert!(!first_dependency.package_root.join(".git").exists());
        assert!(!first_dependency.package_root.join(".ku").exists());
        assert!(!first_dependency.package_root.join("node_modules").exists());
        assert!(!first_dependency.package_root.join("README.md").exists());

        let barrier = Arc::new(Barrier::new(2));
        let reader_barrier = Arc::clone(&barrier);
        let old_root = first_dependency.package_root.clone();
        let reader = thread::spawn(move || {
            reader_barrier.wait();
            let mut contents = Vec::new();
            for index in 0..32 {
                contents.push(
                    fs::read_to_string(old_root.join("src").join(format!("m{index:02}.ku")))
                        .expect("slow reader sees complete old module"),
                );
                thread::sleep(Duration::from_millis(1));
            }
            contents
        });
        barrier.wait();
        for index in 0..32 {
            fs::write(
                source.join("src").join(format!("m{index:02}.ku")),
                format!("fn V{index}(): int {{ return 2 }}\n"),
            )
            .expect("refresh file package module");
        }
        let app_for_refresh = app.clone();
        let refresher = thread::spawn(move || {
            let mut package = discover_from_dir(&app_for_refresh)
                .expect("discover refresh consumer")
                .expect("refresh consumer package");
            resolve_remote_dependencies(&mut package).expect("resolve refreshed snapshot");
            package.resolved_file_dependencies.remove(0)
        });
        let old_contents = reader.join().expect("slow reader thread");
        let refreshed = refresher.join().expect("refresh thread");
        assert!(
            old_contents
                .iter()
                .all(|content| content.contains("return 1")),
            "a concurrent refresh must not mutate the root fixed for an active reader"
        );
        assert_ne!(first_dependency.package_root, refreshed.package_root);
        assert_ne!(first_dependency.checksum, refreshed.checksum);
        assert!(first_dependency.package_root.is_dir());
        assert!(
            fs::read_to_string(refreshed.package_root.join("src").join("m00.ku"))
                .expect("read refreshed module")
                .contains("return 2")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bare_file_dependency_requires_an_exact_version() {
        let root = temp_path("bare-file-version");
        let _ = fs::remove_dir_all(&root);
        let source = root.join("util");
        let app = root.join("app");
        fs::create_dir_all(source.join("src")).expect("create bare file package source");
        fs::create_dir_all(app.join("src")).expect("create consumer source");
        fs::write(
            source.join("src").join("util.ku"),
            b"fn Value(): int { return 1 }\n",
        )
        .expect("write bare file package source");
        let write_manifest = |requirement: &str| {
            fs::write(
                app.join(MANIFEST_FILE),
                format!(
                    "name = \"app\"\nversion = \"0.1.0\"\ndep.util = \"{requirement}\"\ndep.util.source = \"file://{}\"\n",
                    source.to_string_lossy().replace('\\', "/")
                ),
            )
            .expect("write consumer manifest");
        };
        write_manifest("^1.0.0");
        let mut ranged = discover_from_dir(&app)
            .expect("discover ranged consumer")
            .expect("ranged consumer package");
        let err = resolve_remote_dependencies(&mut ranged)
            .expect_err("bare file packages cannot infer a version for a range");
        assert_eq!(
            err.code.as_deref(),
            Some("file_dependency_manifest_required")
        );

        write_manifest("1.0.0");
        let mut exact = discover_from_dir(&app)
            .expect("discover exact consumer")
            .expect("exact consumer package");
        resolve_remote_dependencies(&mut exact).expect("resolve exact bare file package");
        assert_eq!(exact.resolved_file_dependencies[0].version, "1.0.0");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_override_expands_registry_dependencies_and_short_circuits_same_name_registry() {
        let dependency = |name: &str, version: &str| PackageDependency {
            name: name.to_string(),
            version: version.to_string(),
            source: None,
            checksum: None,
        };
        let manifest =
            |name: &str, version: &str, dependencies: Vec<PackageDependency>| RegistryManifest {
                name: name.to_string(),
                version: version.to_string(),
                source: format!("https://registry.example/{name}-{version}.tar.zst"),
                checksum: format!("sha256-{}", "a".repeat(64)),
                dependencies,
            };
        let registry = PackageRegistry {
            url: "https://registry.example/v1/".to_string(),
            public_key: format!("ed25519-{}", "0".repeat(64)),
        };
        let overrides = HashMap::from([(
            "util".to_string(),
            FileOverride {
                version: "1.0.0".to_string(),
                dependencies: vec![dependency("core", "^1.0.0")],
            },
        )]);
        let mut solver = RegistryGraphSolver::with_deadline_and_overrides(
            &registry,
            package_operation_deadline(),
            overrides,
        );
        solver
            .insert_test_index(RegistryIndex {
                name: "a".to_string(),
                versions: vec![manifest("a", "1.0.0", vec![dependency("util", "1.0.0")])],
            })
            .expect("seed a index");
        solver
            .insert_test_index(RegistryIndex {
                name: "core".to_string(),
                versions: vec![manifest("core", "1.2.0", Vec::new())],
            })
            .expect("seed core index");
        solver
            .insert_test_index(RegistryIndex {
                name: "util".to_string(),
                versions: vec![manifest("util", "1.0.0", Vec::new())],
            })
            .expect("seed ignored same-name registry index");
        let solved = solver
            .solve(&[dependency("util", "1.0.0"), dependency("a", "1.0.0")])
            .expect("file override should satisfy both direct and transitive constraints");
        assert_eq!(solved["a"].version, "1.0.0");
        assert_eq!(solved["core"].version, "1.2.0");
        assert!(
            !solved.contains_key("util"),
            "the same-name registry package must not coexist with the file override"
        );

        let mut incompatible = RegistryGraphSolver::with_deadline_and_overrides(
            &registry,
            package_operation_deadline(),
            HashMap::from([(
                "util".to_string(),
                FileOverride {
                    version: "1.0.0".to_string(),
                    dependencies: Vec::new(),
                },
            )]),
        );
        incompatible
            .insert_test_index(RegistryIndex {
                name: "a".to_string(),
                versions: vec![manifest("a", "1.0.0", vec![dependency("util", "2.0.0")])],
            })
            .expect("seed incompatible a index");
        let err = incompatible
            .solve(&[dependency("util", "1.0.0"), dependency("a", "1.0.0")])
            .expect_err("an incompatible transitive constraint must reject the override");
        assert_eq!(err.code.as_deref(), Some("dependency_conflict"));
    }

    #[test]
    fn file_package_transitive_registry_dependency_closes_online_locked_offline_and_native() {
        const TOKEN: &str = "KU_FILE_GRAPH_TOKEN_4H8M2";
        let signing_key = SigningKey::from_bytes(&[61u8; 32]);
        let public_key = signing_key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let (registry, agent) =
            TestHttpsRegistry::start_for(signing_key, TOKEN, "core", "1.0.0", Vec::new());
        let base_url = registry.base_url.clone();
        let registry_state = Arc::clone(&registry.state);
        let _agent_guard = TestRegistryHttpAgentGuard::install(base_url.clone(), agent);
        let token_environment = TestEnvironmentVariableGuard::unset(REGISTRY_TOKEN_ENV);
        token_environment.set(TOKEN);

        let root = temp_path("file-registry-graph-e2e");
        let _ = fs::remove_dir_all(&root);
        let core = root.join("core");
        let util = root.join("util");
        let app = root.join("app");
        for package in [&core, &util, &app] {
            fs::create_dir_all(package.join("src")).expect("create test package source");
        }
        fs::write(
            core.join(MANIFEST_FILE),
            format!(
                "name = \"core\"\nversion = \"1.0.0\"\nregistry.url = \"{base_url}\"\nregistry.public_key = \"ed25519-{public_key}\"\n"
            ),
        )
        .expect("write core manifest");
        fs::write(
            core.join("src").join("core.ku"),
            b"fn Base(): int { return 40 }\n",
        )
        .expect("write core source");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "publish".to_string(),
            core.to_string_lossy().to_string(),
        ])
        .expect("publish core to the signed HTTPS registry");

        fs::write(
            util.join(MANIFEST_FILE),
            "name = \"util\"\nversion = \"1.0.0\"\ndep.core = \"1.0.0\"\n",
        )
        .expect("write file util manifest");
        fs::write(
            util.join("src").join("util.ku"),
            b"import { Base } from \"@core/core\"\nfn Value(): int { return Base() + 2 }\n",
        )
        .expect("write file util source");
        fs::write(
            app.join(MANIFEST_FILE),
            format!(
                "name = \"app\"\nversion = \"0.1.0\"\nregistry.url = \"{base_url}\"\nregistry.public_key = \"ed25519-{public_key}\"\ndep.util = \"1.0.0\"\ndep.util.source = \"file://{}\"\n",
                util.to_string_lossy().replace('\\', "/")
            ),
        )
        .expect("write app manifest");
        let main = app.join("src").join("main.ku");
        fs::write(
            &main,
            b"import { Value } from \"@util/util\"\nfn main() { if (Value() != 42) { panic(\"bad file registry graph\") } }\n",
        )
        .expect("write app source");

        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "resolve".to_string(),
            app.to_string_lossy().to_string(),
        ])
        .expect("resolve file-to-registry graph online");
        let lock_path = app.join(LOCK_FILE);
        let lock = fs::read_to_string(&lock_path).expect("read exact graph lock");
        assert_eq!(lock.matches("name = \"util\"").count(), 1);
        assert_eq!(lock.matches("name = \"core\"").count(), 1);
        assert!(lock.contains("source = \"registry\""));

        let mut resolved_app = discover_from_dir(&app)
            .expect("rediscover file-registry consumer")
            .expect("file-registry consumer package");
        resolve_remote_dependencies_with_mode(&mut resolved_app, DependencyResolveMode::Offline)
            .expect("inspect warm locked graph without network");
        let core_package_root = resolved_app
            .resolved_registry_dependencies
            .iter()
            .find(|dependency| dependency.name == "core")
            .expect("resolved core cache")
            .package_root
            .clone();
        let core_cache = core_package_root
            .parent()
            .expect("registry package root has a cache container")
            .to_path_buf();
        fs::remove_dir_all(&core_cache)
            .expect("remove complete core cache for locked exact-download test");
        let requests_before_locked_download = {
            let state = registry_state
                .lock()
                .expect("test registry state mutex poisoned");
            (
                state.signature_reads,
                state.index_reads,
                state.artifact_reads,
                state.successful_uploads,
            )
        };
        let locked_download_runner = app.join("locked-download-runner.exe");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "build".to_string(),
            "--locked".to_string(),
            "-o".to_string(),
            locked_download_runner.to_string_lossy().to_string(),
            main.to_string_lossy().to_string(),
        ])
        .expect("locked runner build may restore only the exact cached artifact");
        assert_eq!(
            fs::read(&lock_path).expect("read lock after exact locked download"),
            lock.as_bytes(),
            "locked cache restoration must not rewrite ku.lock"
        );
        let requests_after_locked_download = {
            let state = registry_state
                .lock()
                .expect("test registry state mutex poisoned");
            (
                state.signature_reads,
                state.index_reads,
                state.artifact_reads,
                state.successful_uploads,
            )
        };
        assert_eq!(
            requests_after_locked_download.0, requests_before_locked_download.0,
            "locked cache restoration must not fetch a signature"
        );
        assert_eq!(
            requests_after_locked_download.1, requests_before_locked_download.1,
            "locked cache restoration must not fetch an index"
        );
        assert_eq!(
            requests_after_locked_download.2,
            requests_before_locked_download.2 + 1,
            "locked cache restoration must fetch the exact artifact once"
        );
        assert_eq!(
            requests_after_locked_download.3, requests_before_locked_download.3,
            "locked cache restoration must not publish"
        );
        let frozen_lock = fs::read(&lock_path).expect("snapshot frozen graph lock");

        let requests_before_stop = {
            let state = registry_state
                .lock()
                .expect("test registry state mutex poisoned");
            (
                state.signature_reads,
                state.index_reads,
                state.artifact_reads,
                state.successful_uploads,
            )
        };
        drop(registry);

        for mode in ["--locked", "--offline"] {
            crate::cli::run_cli(vec![
                "ku".to_string(),
                "package".to_string(),
                "resolve".to_string(),
                app.to_string_lossy().to_string(),
                mode.to_string(),
            ])
            .unwrap_or_else(|err| panic!("{mode} graph resolve after registry shutdown: {err:?}"));
            assert_eq!(
                fs::read(&lock_path).expect("read lock after frozen graph resolve"),
                frozen_lock,
                "package resolve {mode} must not rewrite ku.lock"
            );
        }
        for mode in ["--locked", "--offline"] {
            crate::cli::run_cli(vec![
                "ku".to_string(),
                "check".to_string(),
                mode.to_string(),
                main.to_string_lossy().to_string(),
            ])
            .unwrap_or_else(|err| panic!("check {mode} after registry shutdown: {err:?}"));
            crate::cli::run_cli(vec![
                "ku".to_string(),
                "run".to_string(),
                main.to_string_lossy().to_string(),
                mode.to_string(),
            ])
            .unwrap_or_else(|err| panic!("run {mode} after registry shutdown: {err:?}"));
            crate::cli::run_cli(vec![
                "ku".to_string(),
                "build".to_string(),
                "--native".to_string(),
                mode.to_string(),
                main.to_string_lossy().to_string(),
            ])
            .unwrap_or_else(|err| panic!("build --native {mode} after registry shutdown: {err:?}"));
            let runner = app.join(format!("{}-runner.exe", mode.trim_start_matches("--")));
            crate::cli::run_cli(vec![
                "ku".to_string(),
                "build".to_string(),
                mode.to_string(),
                "-o".to_string(),
                runner.to_string_lossy().to_string(),
                main.to_string_lossy().to_string(),
            ])
            .unwrap_or_else(|err| panic!("runner build {mode} after shutdown: {err:?}"));
            let status = std::process::Command::new(&runner)
                .status()
                .unwrap_or_else(|err| panic!("start {mode} runner '{}': {err}", runner.display()));
            assert!(status.success(), "{mode} runner exited with {status}");
            assert_eq!(
                fs::read(&lock_path).expect("read unchanged frozen graph lock"),
                frozen_lock,
                "{mode} check/run/native/runner build must not rewrite ku.lock"
            );
        }
        let c = fs::read_to_string(main.with_extension("c")).expect("read native C artifact");
        assert!(c.contains("Base(") && c.contains("Value("));
        assert!(!c.contains("run_source") && !c.contains("const SOURCE"));
        let requests_after_stop = {
            let state = registry_state
                .lock()
                .expect("test registry state mutex poisoned");
            assert!(state.protocol_errors.is_empty(), "{state:?}");
            (
                state.signature_reads,
                state.index_reads,
                state.artifact_reads,
                state.successful_uploads,
            )
        };
        assert_eq!(
            requests_after_stop, requests_before_stop,
            "locked/offline/check/run/native/runner must not touch the stopped registry"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_package_uses_explicit_same_name_file_override_without_duplicate_scope() {
        const TOKEN: &str = "KU_OVERRIDE_TOKEN_7Q2N5";
        let util_requirement = PackageDependency {
            name: "util".to_string(),
            version: "1.0.0".to_string(),
            source: None,
            checksum: None,
        };
        let signing_key = SigningKey::from_bytes(&[73u8; 32]);
        let public_key = signing_key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let (registry, agent) = TestHttpsRegistry::start_for(
            signing_key,
            TOKEN,
            "adapter",
            "1.0.0",
            vec![util_requirement],
        );
        let base_url = registry.base_url.clone();
        let _agent_guard = TestRegistryHttpAgentGuard::install(base_url.clone(), agent);
        let token_environment = TestEnvironmentVariableGuard::unset(REGISTRY_TOKEN_ENV);
        token_environment.set(TOKEN);
        let root = temp_path("same-name-file-override-e2e");
        let _ = fs::remove_dir_all(&root);
        let adapter = root.join("adapter");
        let util = root.join("util");
        let app = root.join("app");
        for package in [&adapter, &util, &app] {
            fs::create_dir_all(package.join("src")).expect("create test package source");
        }
        fs::write(
            adapter.join(MANIFEST_FILE),
            format!(
                "name = \"adapter\"\nversion = \"1.0.0\"\nregistry.url = \"{base_url}\"\nregistry.public_key = \"ed25519-{public_key}\"\ndep.util = \"1.0.0\"\n"
            ),
        )
        .expect("write adapter manifest");
        fs::write(
            adapter.join("src").join("adapter.ku"),
            b"import { Value } from \"@util/util\"\nfn Combined(): int { return Value() + 1 }\n",
        )
        .expect("write adapter source");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "publish".to_string(),
            adapter.to_string_lossy().to_string(),
        ])
        .expect("publish adapter with signed util constraint");

        fs::write(
            util.join(MANIFEST_FILE),
            "name = \"util\"\nversion = \"1.0.0\"\n",
        )
        .expect("write overridden util manifest");
        fs::write(
            util.join("src").join("util.ku"),
            b"fn Value(): int { return 7 }\n",
        )
        .expect("write overridden util value");
        fs::write(
            app.join(MANIFEST_FILE),
            format!(
                "name = \"app\"\nversion = \"0.1.0\"\nregistry.url = \"{base_url}\"\nregistry.public_key = \"ed25519-{public_key}\"\ndep.adapter = \"1.0.0\"\ndep.util = \"1.0.0\"\ndep.util.source = \"file://{}\"\n",
                util.to_string_lossy().replace('\\', "/")
            ),
        )
        .expect("write override consumer manifest");
        let main = app.join("src").join("main.ku");
        fs::write(
            &main,
            b"import { Combined } from \"@adapter/adapter\"\nfn main() { if (Combined() != 8) { panic(\"wrong util instance\") } }\n",
        )
        .expect("write override consumer source");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "package".to_string(),
            "resolve".to_string(),
            app.to_string_lossy().to_string(),
        ])
        .expect("resolve adapter against the explicit file util override");
        let mut resolved = discover_from_dir(&app)
            .expect("rediscover override consumer")
            .expect("override consumer package");
        resolve_remote_dependencies_with_mode(&mut resolved, DependencyResolveMode::Offline)
            .expect("reconstruct the exact override graph from lock and cache");
        assert_eq!(resolved.resolved_file_dependencies.len(), 1);
        assert_eq!(resolved.resolved_file_dependencies[0].name, "util");
        assert_eq!(resolved.resolved_registry_dependencies.len(), 1);
        assert_eq!(resolved.resolved_registry_dependencies[0].name, "adapter");
        let scopes =
            package_import_scopes(&resolved, Span::default()).expect("build unique import scopes");
        assert_eq!(
            scopes
                .iter()
                .filter(|scope| scope.package_name == "util")
                .count(),
            1,
            "the graph must expose exactly one util scope"
        );
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "check".to_string(),
            main.to_string_lossy().to_string(),
        ])
        .expect("check adapter with file util override");
        crate::cli::run_cli(vec![
            "ku".to_string(),
            "run".to_string(),
            main.to_string_lossy().to_string(),
        ])
        .expect("run adapter with file util override value");
        let lock = fs::read_to_string(app.join(LOCK_FILE)).expect("read override graph lock");
        assert_eq!(lock.matches("name = \"util\"").count(), 1);
        drop(registry);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_dependency_limit_counts_unique_names_not_existing_dependency_fields() {
        let mut source = String::from("name = \"app\"\nversion = \"0.1.0\"\n");
        for index in 0..MAX_REGISTRY_DEPENDENCIES {
            source.push_str(&format!("dep.p{index:03} = \"1.0.0\"\n"));
        }
        let file_url = if cfg!(windows) {
            "file://C:/tmp/util"
        } else {
            "file:///tmp/util"
        };
        source.push_str(&format!(
            "dep.p000.source = \"{file_url}\"\ndep.p000.checksum = \"ku-fnv64-0000000000000000\"\n"
        ));
        let manifest = parse_manifest(&source, Span::default())
            .expect("fields for an existing dependency must not consume another graph slot");
        assert_eq!(manifest.dependencies.len(), MAX_REGISTRY_DEPENDENCIES);

        source.push_str("dep.overflow = \"1.0.0\"\n");
        let err = parse_manifest(&source, Span::default())
            .expect_err("the 257th unique dependency must fail during manifest parsing");
        assert_eq!(err.code.as_deref(), Some("dependency_limit"));
    }

    #[test]
    fn registry_index_version_rejects_more_than_the_graph_dependency_limit() {
        let mut source = String::from(
            "name = \"bundle\"\n[[version]]\nversion = \"1.0.0\"\nurl = \"bundle-1.0.0.tar.zst\"\nchecksum = \"sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
        );
        for index in 0..MAX_REGISTRY_DEPENDENCIES {
            source.push_str(&format!("dep.p{index:03} = \"1.0.0\"\n"));
        }
        let index = parse_registry_index(
            &source,
            "https://registry.example/v1/packages/bundle/index.toml",
            Span::default(),
        )
        .expect("the signed index boundary must remain accepted");
        assert_eq!(
            index.versions[0].dependencies.len(),
            MAX_REGISTRY_DEPENDENCIES
        );

        source.push_str("dep.overflow = \"1.0.0\"\n");
        let err = parse_registry_index(
            &source,
            "https://registry.example/v1/packages/bundle/index.toml",
            Span::default(),
        )
        .expect_err("a signed version with 257 dependencies must fail closed");
        assert_eq!(err.code.as_deref(), Some("dependency_limit"));
    }

    #[test]
    fn pack_rechecks_dependency_limit_for_programmatically_constructed_manifest() {
        let root = temp_path("pack-dependency-limit");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("create package source");
        fs::write(
            root.join(MANIFEST_FILE),
            "name = \"bundle\"\nversion = \"1.0.0\"\n",
        )
        .expect("write package manifest");
        fs::write(
            root.join("src").join("bundle.ku"),
            b"fn Value(): int { return 1 }\n",
        )
        .expect("write package source");
        let mut package = discover_from_dir(&root)
            .expect("discover package")
            .expect("package context");
        package.manifest.dependencies = (0..=MAX_REGISTRY_DEPENDENCIES)
            .map(|index| PackageDependency {
                name: format!("p{index:03}"),
                version: "1.0.0".to_string(),
                source: None,
                checksum: None,
            })
            .collect();
        let err = pack_package(&package)
            .expect_err("pack must not trust a programmatically constructed oversized manifest");
        assert_eq!(err.code.as_deref(), Some("dependency_limit"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_solver_rejects_cumulative_multi_index_resource_overflow() {
        let registry = PackageRegistry {
            url: "https://registry.example/v1/".to_string(),
            public_key: format!("ed25519-{}", "0".repeat(64)),
        };
        let index = |name: &str| RegistryIndex {
            name: name.to_string(),
            versions: Vec::new(),
        };
        let mut solver = RegistryGraphSolver::new(&registry);
        let first_bytes = MAX_REGISTRY_GRAPH_INDEX_BYTES / 2;
        solver
            .insert_test_index_with_source_bytes(index("first"), first_bytes)
            .expect("first signed index must fit the graph byte budget");
        solver
            .insert_test_index_with_source_bytes(
                index("second"),
                MAX_REGISTRY_GRAPH_INDEX_BYTES - first_bytes,
            )
            .expect("multiple indexes may exactly fill the graph byte budget");
        let err = solver
            .insert_test_index_with_source_bytes(index("overflow"), 1)
            .expect_err("one more signed index byte must fail before it is cached");
        assert_eq!(err.code.as_deref(), Some("registry_index_graph_limit"));
        assert!(!solver.indexes.contains_key("overflow"));

        let mut version_budget = RegistryIndexGraphBudget::default();
        let first_versions = MAX_REGISTRY_GRAPH_INDEX_VERSIONS / 2;
        version_budget
            .record(RegistryIndexResourceUsage {
                source_bytes: 0,
                versions: first_versions,
                parsed_items: first_versions,
            })
            .expect("first version batch fits");
        version_budget
            .record(RegistryIndexResourceUsage {
                source_bytes: 0,
                versions: MAX_REGISTRY_GRAPH_INDEX_VERSIONS - first_versions,
                parsed_items: MAX_REGISTRY_GRAPH_INDEX_VERSIONS - first_versions,
            })
            .expect("multiple indexes may exactly fill the version budget");
        let err = version_budget
            .record(RegistryIndexResourceUsage {
                source_bytes: 0,
                versions: 1,
                parsed_items: 1,
            })
            .expect_err("one more parsed version must fail");
        assert_eq!(err.code.as_deref(), Some("registry_index_graph_limit"));

        let mut item_budget = RegistryIndexGraphBudget::default();
        let first_items = MAX_REGISTRY_GRAPH_INDEX_ITEMS / 2;
        item_budget
            .record(RegistryIndexResourceUsage {
                source_bytes: 0,
                versions: 0,
                parsed_items: first_items,
            })
            .expect("first parsed item batch fits");
        item_budget
            .record(RegistryIndexResourceUsage {
                source_bytes: 0,
                versions: 0,
                parsed_items: MAX_REGISTRY_GRAPH_INDEX_ITEMS - first_items,
            })
            .expect("multiple indexes may exactly fill the parsed item budget");
        let err = item_budget
            .record(RegistryIndexResourceUsage {
                source_bytes: 0,
                versions: 0,
                parsed_items: 1,
            })
            .expect_err("one more parsed item must fail");
        assert_eq!(err.code.as_deref(), Some("registry_index_graph_limit"));
    }

    #[test]
    fn archive_and_cache_tree_stop_at_remaining_plus_one_before_sorting() {
        let root = temp_path("bounded-read-dir");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("create bounded package source");
        fs::write(
            root.join(MANIFEST_FILE),
            "name = \"bounded\"\nversion = \"1.0.0\"\n",
        )
        .expect("write bounded manifest");
        fs::write(root.join("src").join("a.ku"), b"fn A() {}\n").expect("write first source");
        fs::write(root.join("src").join("b.ku"), b"fn B() {}\n").expect("write second source");
        let package = discover_from_dir(&root)
            .expect("discover bounded package")
            .expect("bounded package context");
        let err = collect_package_archive_entries(
            &package,
            PackageArchivePolicy {
                max_files: 4,
                ..PackageArchivePolicy::default()
            },
            Span::default(),
        )
        .expect_err("pack collection must stop on the first entry beyond remaining budget");
        assert_eq!(err.code.as_deref(), Some("package_archive_limit"));
        let err = package_tree_sha256(
            &root.join("src"),
            PackageArchivePolicy {
                max_files: 1,
                ..PackageArchivePolicy::default()
            },
            Span::default(),
        )
        .expect_err("cache tree hashing must stop before unbounded directory collection");
        assert_eq!(err.code.as_deref(), Some("registry_cache_tampered"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_snapshot_exclusions_are_case_insensitive_and_scanned_entries_are_bounded() {
        let root = temp_path("file-snapshot-excluded-budget");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("create snapshot source");
        fs::write(
            root.join(MANIFEST_FILE),
            "name = \"snapshot\"\nversion = \"1.0.0\"\n",
        )
        .expect("write snapshot manifest");
        fs::write(
            root.join("src").join("main.ku"),
            b"fn Value(): int { return 1 }\n",
        )
        .expect("write snapshot source");
        let checksum = package_source_checksum(&root).expect("hash clean snapshot");
        fs::write(root.join("src").join(".Env.Local"), b"SECRET=one")
            .expect("write mixed-case env file");
        fs::create_dir_all(root.join("src").join(".GIT")).expect("create mixed-case git dir");
        fs::write(root.join("src").join(".GIT").join("config"), b"token=one")
            .expect("write excluded git file");
        assert_eq!(
            package_source_checksum(&root).expect("hash snapshot with excluded entries"),
            checksum
        );
        for index in 0..MAX_PACKAGE_FILES {
            fs::write(
                root.join("src").join(format!(".ENV.attack-{index:04}")),
                b"x",
            )
            .expect("write excluded attack entry");
        }
        let err = package_source_checksum(&root)
            .expect_err("excluded names must not bypass the scanned-entry budget");
        assert_eq!(err.code.as_deref(), Some("checksum_limit"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dependency_keys_have_one_canonical_prefix_and_duplicates_fail() {
        let non_canonical =
            parse_manifest("name = \"app\"\ndep.dep.foo = \"1.0.0\"\n", Span::default())
                .expect_err("a repeated dep prefix must not alias dep.foo");
        assert!(matches!(
            non_canonical.code.as_deref(),
            Some("invalid_dependency_field") | Some("invalid_dependency_key")
        ));
        let duplicate = parse_manifest(
            "name = \"app\"\ndep.foo = \"1.0.0\"\ndep.foo = \"1.0.0\"\n",
            Span::default(),
        )
        .expect_err("a dependency must have one canonical version field");
        assert_eq!(duplicate.code.as_deref(), Some("duplicate_manifest_key"));
        let index = "name = \"app\"\n[[version]]\nversion = \"1.0.0\"\nurl = \"app-1.0.0.tar.zst\"\nchecksum = \"sha256-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\ndep.dep.foo = \"1.0.0\"\n";
        let err = parse_registry_index(
            index,
            "https://registry.example/v1/packages/app/index.toml",
            Span::default(),
        )
        .expect_err("signed indexes must reject repeated dep prefixes");
        assert_eq!(err.code.as_deref(), Some("invalid_registry_dependency"));
    }

    #[test]
    fn lock_hashing_and_atomic_write_enforce_their_exact_byte_limits() {
        let root = temp_path("lock-byte-limits");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("create lock test source");
        fs::write(root.join(MANIFEST_FILE), "name = \"app\"\n").expect("write lock test manifest");
        let oversized_dependency = root.join("src").join("large.ku");
        fs::write(
            &oversized_dependency,
            vec![b'x'; (MAX_LOCKED_DEPENDENCY_BYTES + 1) as usize],
        )
        .expect("write oversized dependency");
        let package = discover_from_dir(&root)
            .expect("discover lock test package")
            .expect("lock test package");
        let err = lock_dependencies(&package, &[oversized_dependency])
            .expect_err("dependency hashing must reject one byte past its bound");
        assert_eq!(err.code.as_deref(), Some("lock_dependency_limit"));

        let mut boundary = "x".repeat(MAX_PACKAGE_LOCK_BYTES as usize - 1);
        append_package_lock_section(&mut boundary, "y").expect("exact lock boundary is accepted");
        let err = append_package_lock_section(&mut boundary, "z")
            .expect_err("one byte past the generated lock boundary must fail");
        assert_eq!(err.code.as_deref(), Some("package_lock_limit"));
        let lock_path = root.join("oversized.lock");
        let err = write_package_lock_atomically(
            &lock_path,
            &vec![b'x'; MAX_PACKAGE_LOCK_BYTES as usize + 1],
        )
        .expect_err("atomic lock replacement must independently enforce the write bound");
        assert_eq!(err.code.as_deref(), Some("package_lock_limit"));
        assert!(!lock_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_and_registry_usage_share_one_graph_resource_budget() {
        let mut budget = RegistryGraphResourceBudget::default();
        budget
            .record_file_snapshot(MAX_PACKAGE_BYTES)
            .expect("file snapshot fits initial graph budget");
        for _ in 0..15 {
            budget
                .record_registry_usage(RegistryPackageResourceUsage {
                    compressed: MAX_REGISTRY_DOWNLOAD_BYTES,
                    unpacked: 0,
                })
                .expect("registry packages share remaining graph budget");
        }
        budget
            .record_registry_usage(RegistryPackageResourceUsage {
                compressed: 22_000_000,
                unpacked: 0,
            })
            .expect("mixed graph accepts the exact compressed-equivalent boundary");
        assert_eq!(budget.compressed, MAX_REGISTRY_GRAPH_DOWNLOAD_BYTES);
        let err = budget
            .record_registry_usage(RegistryPackageResourceUsage {
                compressed: 1,
                unpacked: 0,
            })
            .expect_err("file and registry usage must not receive independent graph budgets");
        assert_eq!(err.code.as_deref(), Some("dependency_graph_size_limit"));
    }

    #[test]
    fn gc_drops_registry_cache_from_stale_lock_after_manifest_root_is_removed() {
        let root = temp_path("gc-stale-registry-root");
        let _ = fs::remove_dir_all(&root);
        let library = root.join("library");
        let app = root.join("app");
        fs::create_dir_all(library.join("src")).expect("create library source");
        fs::create_dir_all(app.join("src")).expect("create app source");
        fs::write(
            library.join(MANIFEST_FILE),
            "name = \"library\"\nversion = \"1.0.0\"\n",
        )
        .expect("write library manifest");
        fs::write(
            library.join("src").join("lib.ku"),
            b"fn Value(): int { return 1 }\n",
        )
        .expect("write library source");
        let artifact = pack_package(
            &discover_from_dir(&library)
                .expect("discover library")
                .expect("library package"),
        )
        .expect("pack library");
        let public_key = SigningKey::from_bytes(&[81u8; 32])
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        fs::write(
            app.join(MANIFEST_FILE),
            format!(
                "name = \"app\"\nversion = \"0.1.0\"\nregistry.url = \"https://registry.example/v1/\"\nregistry.public_key = \"ed25519-{public_key}\"\ndep.library = \"1.0.0\"\n"
            ),
        )
        .expect("write app manifest");
        let mut package = discover_from_dir(&app)
            .expect("discover app")
            .expect("app package");
        let (manifest, package_root, cache_key) =
            seed_registry_artifact(&package.cache_dir, &artifact, Vec::new());
        let cache_target = package_root.parent().expect("cache target").to_path_buf();
        package.resolved_registry_dependencies = vec![ResolvedPackageDependency {
            name: manifest.name.clone(),
            requirement: "1.0.0".to_string(),
            version: manifest.version.clone(),
            registry: "https://registry.example/v1/".to_string(),
            url: manifest.source.clone(),
            checksum: manifest.checksum.clone(),
            cache_key,
            package_root,
        }];
        write_lock(&package).expect("write registry lock");
        let mut exhausted_budget = CacheGcScanBudget {
            remaining_entries: 0,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let conservative = gc_locked_cache_keep_set(&package, &mut exhausted_budget)
            .expect("budget exhaustion is a normal conservative GC stop");
        assert!(conservative.contains(&cache_target));
        fs::write(
            app.join(MANIFEST_FILE),
            format!(
                "name = \"app\"\nversion = \"0.1.0\"\nregistry.url = \"https://registry.example/v1/\"\nregistry.public_key = \"ed25519-{public_key}\"\n"
            ),
        )
        .expect("remove registry root from manifest");
        let package = discover_from_dir(&app)
            .expect("rediscover app")
            .expect("updated app package");
        let removed = gc_cache(&package, 8).expect("GC stale lock cache");
        assert_eq!(removed, 1);
        assert!(!cache_target.exists());
        let _ = fs::remove_dir_all(root);
    }
}
