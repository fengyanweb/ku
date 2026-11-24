//! Build-time extraction runs against the selected immutable GitHub release.
#[path = "src/browser/generate_import_support.rs"]
mod browser_import_support;
fn main() {
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    browser_import_support::generate(&root, &out);
    println!("cargo:rerun-if-changed=src/browser/generate_import_support.rs");
    println!("cargo:rerun-if-changed=src/cli.rs");
}
