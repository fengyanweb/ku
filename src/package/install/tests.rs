use super::*;

fn project_fixture() -> TemporaryDirectory {
    let path = env::temp_dir().join(format!("ku-install-test-{}", operation_nonce()));
    fs::create_dir(&path).unwrap();
    TemporaryDirectory::new(path)
}

fn fixture_package(path: &Path, name: &str) {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(
        path.join("ku.mod"),
        format!("name = \"{name}\"\nversion = \"1.0.0\"\n"),
    )
    .unwrap();
    fs::write(
        path.join("src").join("value.ku"),
        "fn Value(): int { return 42 }\nfn hidden(): int { return 1 }\n",
    )
    .unwrap();
}

fn context(project: &Path) -> PackageContext {
    discover_from_dir(project).unwrap().unwrap()
}

#[test]
fn relative_sources_install_and_restore_after_moving_the_project() {
    let temp = project_fixture();
    let checkout = temp.path.join("before");
    fixture_package(&checkout, "consumer");
    fixture_package(&checkout.join("vendor").join("core"), "core");
    fs::write(checkout.join("vendor/core/LICENSE"), "test notice\n").unwrap();
    let mut project = context(&checkout);

    let receipts = install_dependency(
        &mut project,
        Some("./vendor/core"),
        false,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].source, "path:vendor/core");
    assert_eq!(receipts[0].imports[0].path, "@core/value");
    assert_eq!(receipts[0].imports[0].exports, ["Value"]);
    assert_eq!(
        fs::read_to_string(receipts[0].package_root.join("LICENSE")).unwrap(),
        "test notice\n"
    );
    let lock = fs::read(&project.lock_path).unwrap();
    let lock_text = std::str::from_utf8(&lock).unwrap();
    assert!(lock_text.contains("checksum = \"sha256-"));
    assert!(lock_text.contains("source = \"path:vendor/core\""));
    assert!(!lock_text.contains(checkout.to_str().unwrap()));

    install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap();
    assert_eq!(fs::read(&project.lock_path).unwrap(), lock);
    fs::remove_dir_all(&project.cache_dir).unwrap();
    let moved = temp.path.join("after");
    fs::rename(&checkout, &moved).unwrap();
    let mut project = context(&moved);
    let offline = install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Offline,
    )
    .unwrap_err();
    assert_eq!(offline.code.as_deref(), Some("offline_cache_miss"));
    assert!(offline.message.contains("core"));
    install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Locked,
    )
    .unwrap();
    assert_eq!(fs::read(&project.lock_path).unwrap(), lock);
    let installed = &project.resolved_file_dependencies[0];
    fs::write(installed.package_root.join("src/value.ku"), "tampered").unwrap();
    let tampered = install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Offline,
    )
    .unwrap_err();
    assert_eq!(tampered.code.as_deref(), Some("file_cache_tampered"));
}

#[test]
fn production_restore_does_not_download_dev_or_discard_its_lock() {
    let root = project_fixture();
    fixture_package(&root.path, "consumer");
    fixture_package(&root.path.join("vendor/core"), "core");
    fixture_package(&root.path.join("vendor/testkit"), "testkit");
    let mut project = context(&root.path);
    install_dependency(
        &mut project,
        Some("./vendor/core"),
        false,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap();
    install_dependency(
        &mut project,
        Some("./vendor/testkit"),
        true,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap();
    assert!(fs::read_to_string(&project.manifest_path)
        .unwrap()
        .contains("dev.testkit = \"1.0.0\""));
    let lock = fs::read(&project.lock_path).unwrap();
    let dev_cache = project
        .resolved_file_dependencies
        .iter()
        .find(|dep| dep.name == "testkit")
        .unwrap()
        .package_root
        .clone();
    fs::remove_dir_all(&project.cache_dir).unwrap();
    let restored = install_dependency(
        &mut project,
        None,
        false,
        true,
        DependencyResolveMode::Locked,
    )
    .unwrap();
    assert_eq!(
        restored
            .iter()
            .map(|package| package.name.as_str())
            .collect::<Vec<_>>(),
        ["core"]
    );
    assert!(!dev_cache.exists());
    assert_eq!(fs::read(&project.lock_path).unwrap(), lock);

    let source = fs::read_to_string(&project.manifest_path).unwrap();
    fs::write(
        &project.manifest_path,
        format!("{source}\n# testing a direct constraint change\nmain = \"other.ku\"\n"),
    )
    .unwrap();
    let error = install_dependency(
        &mut project,
        None,
        false,
        true,
        DependencyResolveMode::Locked,
    )
    .unwrap_err();
    assert_eq!(error.code.as_deref(), Some("lock_out_of_date"));
    assert_eq!(fs::read(&project.lock_path).unwrap(), lock);
}

#[test]
fn failed_install_hook_preserves_both_files_and_committed_journal_recovers() {
    let root = project_fixture();
    fixture_package(&root.path, "consumer");
    fixture_package(&root.path.join("vendor/foreign"), "foreign");
    let mut project = context(&root.path);
    install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap();
    let original = fs::read(&project.manifest_path).unwrap();
    let lock = fs::read(&project.lock_path).unwrap();
    let rejected = install_dependency_with(
        &mut project,
        Some("./vendor/foreign"),
        false,
        false,
        DependencyResolveMode::Update,
        |candidate, receipts| {
            assert_eq!(candidate.manifest.dependencies[0].name, "foreign");
            assert_eq!(receipts[0].imports[0].path, "@foreign/value");
            Err(error("test_pre_commit", "foreign bridge was unavailable"))
        },
    )
    .unwrap_err();
    assert_eq!(rejected.code.as_deref(), Some("test_pre_commit"));
    assert_eq!(fs::read(&project.manifest_path).unwrap(), original);
    assert_eq!(fs::read(&project.lock_path).unwrap(), lock);
    assert!(!root.path.join(INSTALL_TRANSACTION).exists());

    let mut after = original.clone();
    after.extend(b"main = \"another.ku\"\n");
    let lock_after = b"# completed transaction\n";
    fs::write(
        root.path.join(INSTALL_TRANSACTION),
        transaction_bytes(&original, Some(&lock), &after, lock_after),
    )
    .unwrap();
    fs::write(&project.lock_path, lock_after).unwrap();
    let project = context(&root.path);
    assert_eq!(project.manifest.main.as_deref(), Some("another.ku"));
    assert_eq!(fs::read(&project.manifest_path).unwrap(), after);
    assert_eq!(fs::read(&project.lock_path).unwrap(), lock_after);
    assert!(!root.path.join(INSTALL_TRANSACTION).exists());
}

#[test]
fn git_pins_and_manifest_quoted_fragments_are_validated() {
    let revision = "a".repeat(40);
    let text = format!("https://example.test/owner/repo.git#{revision}:examples/hello");
    let request = GitSource::parse(&text, false, Span::default()).unwrap();
    assert_eq!(request.pin(&revision), format!("git+{text}"));
    let manifest = format!("name = \"consumer\"\ndep.hello = \"1.0.0\"\ndep.hello.source = \"git+{text}\" # a comment\n");
    assert_eq!(
        parse_manifest(&manifest, Span::default())
            .unwrap()
            .dependencies[0]
            .source
            .as_deref(),
        Some(format!("git+{text}").as_str())
    );
    for bad in [
        "https://user:pwd@example.test/repo.git",
        "https://example.test/repo.git#-x",
        "https://example.test/repo.git#HEAD:../x",
        "https://example.test/repo.git#HEAD:/x",
        "https://example.test/repo.git#HEAD:c:\\private",
    ] {
        assert!(
            GitSource::parse(bad, false, Span::default()).is_err(),
            "{bad}"
        );
    }
    assert!(GitSource::parse(
        "git+https://example.test/repo.git#v1",
        true,
        Span::default()
    )
    .is_err());
}

#[test]
fn signed_interop_artifact_is_a_source_only_capability() {
    let root = project_fixture();
    fixture_package(&root.path, "foreign");
    fs::write(
        root.path.join("ku.mod"),
        "name = \"foreign\"\nversion = \"1.0.0\"\ninterop.version = \"1\"\n",
    )
    .unwrap();
    fs::write(root.path.join("src/ku.interop"), "kind = \"native\"\n").unwrap();
    fs::write(
        root.path.join("src/vendor.c"),
        "int value(void) { return 9; }\n",
    )
    .unwrap();
    assert!(pack_package(&context(&root.path)).is_ok());
    fs::write(root.path.join("src/injected.dll"), [0, 1, 2]).unwrap();
    let error = pack_package(&context(&root.path)).unwrap_err();
    assert_eq!(
        error.code.as_deref(),
        Some("unsupported_interop_package_file")
    );
}

#[test]
#[ignore = "requires unauthenticated access to public GitHub"]
fn public_git_install_is_pinned_buildable_and_restorable() {
    let root = project_fixture();
    fixture_package(&root.path, "consumer");
    let mut project = context(&root.path);
    let reference = "https://github.com/fengyanweb/ku.git#540267bfd362211fd91d154c6d6c92d545834569:.github/fixtures/native-three-os";
    let receipts = install_dependency(
        &mut project,
        Some(reference),
        false,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap();
    assert_eq!(receipts[0].source, format!("git+{reference}"));
    assert_eq!(receipts[0].name, "native_ci");
    let exported = receipts[0]
        .imports
        .iter()
        .find(|module| module.exports.iter().any(|name| name == "Add"))
        .unwrap();
    let source = format!("import {{ Add }} from {:?}\nfn main() {{ if (Add(20, 22) != 42) {{ panic \"wrong installed result\" }} println(Add(20, 22)) }}\n", exported.path);
    let path = root.path.join("src/main.ku");
    fs::write(&path, &source).unwrap();
    crate::cli::run_source(path.to_str().unwrap(), &source).unwrap();
    let lock = fs::read(&project.lock_path).unwrap();
    fs::remove_dir_all(&project.cache_dir).unwrap();
    install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Locked,
    )
    .unwrap();
    assert_eq!(fs::read(&project.lock_path).unwrap(), lock);
    crate::cli::run_source(path.to_str().unwrap(), &source).unwrap();
}

#[test]
fn nested_relative_source_is_owned_by_its_parent_and_rebuilds_cold() {
    let root = project_fixture();
    fixture_package(&root.path, "consumer");
    let parent = root.path.join("vendor/parent");
    fixture_package(&parent, "parent");
    let child = parent.join("src/vendor/child");
    fixture_package(&child, "child");
    fs::write(
        parent.join("ku.mod"),
        "name = \"parent\"\nversion = \"1.0.0\"\n\
         dep.child = \"1.0.0\"\n\
         dep.child.source = \"path:src/vendor/child\"\n",
    )
    .unwrap();
    fs::write(
        parent.join("src/value.ku"),
        "import { Value as ChildValue } from \"@child/value\"\nfn Value(): int { return ChildValue() }\n",
    )
    .unwrap();
    let source = "import { Value } from \"@parent/value\"\nfn main() { if (Value() != 42) { panic \"wrong nested result\" } println(Value()) }\n";
    let entry = root.path.join("src/main.ku");
    fs::write(&entry, source).unwrap();

    let mut project = context(&root.path);
    let receipt = install_dependency(
        &mut project,
        Some("./vendor/parent"),
        false,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap();
    assert_eq!(receipt.len(), 1);
    assert_eq!(
        project
            .resolved_file_dependencies
            .iter()
            .map(|dep| dep.name.as_str())
            .collect::<Vec<_>>(),
        ["child", "parent"],
    );
    let child_cache = project.resolved_file_dependencies[0].package_root.clone();
    crate::cli::run_source(entry.to_str().unwrap(), source).unwrap();
    let installed_lock = fs::read(&project.lock_path).unwrap();
    assert_eq!(gc_cache(&project, 8).unwrap(), 0);

    fs::remove_dir_all(&child_cache).unwrap();
    install_dependency(
        &mut project,
        None,
        false,
        true,
        DependencyResolveMode::Locked,
    )
    .unwrap();
    assert!(child_cache.join("src/value.ku").is_file());
    assert_eq!(fs::read(&project.lock_path).unwrap(), installed_lock);
    crate::cli::run_source(entry.to_str().unwrap(), source).unwrap();
}

#[test]
fn nested_dependency_rejects_absent_author_machine_paths_before_installing() {
    let root = project_fixture();
    fixture_package(&root.path, "consumer");
    let parent = root.path.join("vendor/parent");
    fixture_package(&parent, "parent");
    let source = if cfg!(windows) {
        "file://C:/author/local-only"
    } else {
        "file:///author/local-only"
    };
    fs::write(
        parent.join("ku.mod"),
        format!("name = \"parent\"\nversion = \"1.0.0\"\ndep.child = \"1.0.0\"\ndep.child.source = {source:?}\n"),
    ).unwrap();
    let mut project = context(&root.path);
    let error = install_dependency(
        &mut project,
        Some("./vendor/parent"),
        false,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap_err();
    assert_eq!(
        error.code.as_deref(),
        Some("unsupported_nested_dependency_source")
    );
    assert!(!fs::read_to_string(&project.manifest_path)
        .unwrap()
        .contains("dep.parent"));
    assert!(!project.lock_path.exists());
}

#[test]
fn locked_install_rejects_changed_relative_source_and_ordinary_install_refreshes_only_it() {
    let root = project_fixture();
    fixture_package(&root.path, "consumer");
    let local = root.path.join("vendor/core");
    fixture_package(&local, "core");
    fixture_package(&root.path.join("vendor/sibling"), "sibling");
    let mut project = context(&root.path);
    for source in ["./vendor/core", "./vendor/sibling"] {
        install_dependency(
            &mut project,
            Some(source),
            false,
            false,
            DependencyResolveMode::Update,
        )
        .unwrap();
    }
    let lock = fs::read(&project.lock_path).unwrap();
    let installed = project.resolved_file_dependencies[0].package_root.clone();
    let unaffected = project.resolved_file_dependencies[1].clone();
    fs::write(
        local.join("src/value.ku"),
        "fn Value(): int { return 43 }\n",
    )
    .unwrap();
    let err = install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Locked,
    )
    .unwrap_err();
    assert_eq!(err.code.as_deref(), Some("locked_local_source_changed"));
    assert_eq!(fs::read(&project.lock_path).unwrap(), lock);

    install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Offline,
    )
    .unwrap();
    assert_eq!(
        project.resolved_file_dependencies[0].package_root,
        installed
    );

    install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Update,
    )
    .unwrap();
    assert_ne!(fs::read(&project.lock_path).unwrap(), lock);
    assert_ne!(
        project.resolved_file_dependencies[0].package_root,
        installed
    );
    assert_eq!(&project.resolved_file_dependencies[1], &unaffected);
}

#[test]
fn two_installs_in_one_project_preserve_both_direct_dependencies() {
    let root = project_fixture();
    fixture_package(&root.path, "consumer");
    fixture_package(&root.path.join("vendor/core"), "core");
    fixture_package(&root.path.join("vendor/util"), "util");
    let path = &root.path;
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|threads| {
        let start = &start;
        let first = threads.spawn(move || {
            let mut project = context(path);
            start.wait();
            install_dependency(
                &mut project,
                Some("./vendor/core"),
                false,
                false,
                DependencyResolveMode::Update,
            )
            .unwrap();
        });
        let second = threads.spawn(move || {
            let mut project = context(path);
            start.wait();
            install_dependency(
                &mut project,
                Some("./vendor/util"),
                false,
                false,
                DependencyResolveMode::Update,
            )
            .unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();
    });
    let mut project = context(&root.path);
    let restored = install_dependency(
        &mut project,
        None,
        false,
        false,
        DependencyResolveMode::Locked,
    )
    .unwrap();
    assert_eq!(
        restored
            .iter()
            .map(|receipt| receipt.name.as_str())
            .collect::<Vec<_>>(),
        ["core", "util"]
    );
    assert!(!root.path.join(INSTALL_TRANSACTION).exists());
}
