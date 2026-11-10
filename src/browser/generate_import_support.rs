//! Build-time reuse of the selected release's native import semantics.
//! No native loader is compiled into the browser. These exact-marker edits
//! replace only filesystem/package access with the bounded virtual host.
use std::{fs, path::Path};

fn part<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let (_, rest) = source
        .split_once(start)
        .expect("release import start marker");
    let length = rest
        .find(end)
        .unwrap_or_else(|| panic!("release import end marker {end:?} after {start:?}"));
    &source[source.len() - rest.len() - start.len()..source.len() - rest.len() + length]
}

fn replace(source: &mut String, before: &str, after: &str) {
    assert_eq!(
        source.matches(before).count(),
        1,
        "review release import adapter: {before}"
    );
    *source = source.replacen(before, after, 1);
}

pub fn generate(root: &Path, out: &Path) {
    let source = fs::read_to_string(root.join("src/cli.rs")).expect("release CLI import source");
    let mut code =
        String::from("// Generated from this build's selected Ku release. Do not edit.\n");
    code.push_str(part(
        &source,
        "fn deny_unused_imports(",
        "fn parse_and_expand_with_dependency_mode(",
    ));
    code.push_str(part(
        &source,
        "fn is_std_import_path(",
        "struct ModuleExports",
    ));
    code.push_str(part(
        &source,
        "struct ModuleExports",
        "fn read_import_source(",
    ));
    code.push_str(part(
        &source,
        "fn check_library_program(",
        "fn reject_large_file(",
    ));
    code.push_str(part(&source, "fn top_level_names(", "fn namespace_lookup("));
    code.push_str(part(&source, "fn namespace_lookup(", "\n}\n"));
    code.push_str("\n}\n");
    // Native-only helpers are outside the browser adapter's call graph.
    replace(
        &mut code,
        part(
            &source,
            "fn unused_binding_error(",
            "fn unused_import_error(",
        ),
        "",
    );
    replace(
        &mut code,
        part(&source, "    fn load_virtual_entry(", "    fn load_entry("),
        "",
    );
    replace(
        &mut code,
        "struct ModuleLoader {",
        "struct ModuleLoader<'a> {",
    );
    replace(&mut code,
        "    package: Option<PackageContext>,\n    package_import_scopes: Vec<package::PackageImportScope>,\n    dependency_snapshots: Vec<package::LockDependency>,",
        "    files: &'a HashMap<String, String>,");
    replace(
        &mut code,
        "impl ModuleLoader {",
        "impl<'a> ModuleLoader<'a> {",
    );
    replace(&mut code,
        "    fn new(package: Option<PackageContext>) -> KuResult<Self> {\n        let package_import_scopes = package\n            .as_ref()\n            .map(|package| package::package_import_scopes(package, Span::default()))\n            .transpose()?\n            .unwrap_or_default();",
        "    fn new(files: &'a HashMap<String, String>) -> KuResult<Self> {");
    replace(&mut code,
        "            package,\n            package_import_scopes,\n            dependency_snapshots: Vec::new(),",
        "            files,");
    replace(
        &mut code,
        "read_import_source(&canonical, span)?",
        "read_import_source(self.files, &canonical, span)?",
    );
    replace(&mut code,
        "            let dependency_snapshot =\n                package::freeze_lock_dependency(&canonical, source.as_bytes())?;", "");
    replace(
        &mut code,
        "            self.dependency_snapshots.push(dependency_snapshot);",
        "",
    );
    replace(
        &mut code,
        "                self.package.as_ref(),\n                &self.package_import_scopes,\n",
        "",
    );
    replace(
        &mut code,
        "fn std_import_modules(import:",
        "fn native_std_import_modules(import:",
    );
    // AST clone work and import processing share the interpreter's sticky fuel.
    replace(&mut code, "    fn charge_source(&mut self, bytes: usize, span: Span) -> KuResult<()> {",
        "    fn charge_source(&mut self, bytes: usize, span: Span) -> KuResult<()> {\n        super::charge(bytes, span)?;");
    replace(&mut code, "    fn charge_items(&mut self, count: usize, span: Span) -> KuResult<()> {",
        "    fn charge_items(&mut self, count: usize, span: Span) -> KuResult<()> {\n        super::charge(count, span)?;");
    replace(&mut code, "    fn charge_clone(&mut self, material: ExpandedModuleMaterial, span: Span) -> KuResult<()> {",
        "    fn charge_clone(&mut self, material: ExpandedModuleMaterial, span: Span) -> KuResult<()> {\n        super::charge(material.source_bytes as usize, span)?;");
    assert!(
        !code.contains("package::"),
        "native package access in virtual imports"
    );
    assert!(
        !code.contains("fs::"),
        "native filesystem access in virtual imports"
    );
    fs::write(out.join("browser_import_support.rs"), code)
        .expect("write portable import implementation");
    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=src/browser/generate_import_support.rs");
}
