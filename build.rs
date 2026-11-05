#[path = "build_support/build_identity.rs"]
mod build_identity;
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path, process::Command};

fn files(root: &Path, dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read build identity sources")
        .map(|entry| entry.expect("source entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path).expect("source metadata");
        if metadata.is_dir() {
            files(root, &path, out);
        } else if metadata.is_file() {
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
}

fn main() {
    let root = std::path::PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let rustc = Command::new(env::var_os("RUSTC").unwrap())
        .arg("-vV")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .output()
        .expect("probe actual build rustc");
    assert!(rustc.status.success(), "actual build rustc probe failed");
    let version = String::from_utf8(rustc.stdout).expect("rustc UTF-8 identity");
    for (key, field) in [
        ("RELEASE", "release"),
        ("COMMIT", "commit-hash"),
        ("DATE", "commit-date"),
        ("HOST", "host"),
    ] {
        let value = version
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{field}: ")))
            .expect("complete rustc identity");
        println!("cargo:rustc-env=KU_BUILD_RUSTC_{key}={value}");
    }
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|r| r.status.success())
        .and_then(|r| String::from_utf8(r.stdout).ok())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=KU_BUILD_SOURCE_COMMIT={}", commit.trim());
    // Watch both worktree HEAD and its actual branch ref; a commit alone must
    // invalidate identity even when all source bytes stay the same.
    let branch = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|r| r.status.success())
        .and_then(|r| String::from_utf8(r.stdout).ok());
    for name in std::iter::once("HEAD".to_string()).chain(branch.map(|s| s.trim().to_string())) {
        if let Ok(result) = Command::new("git")
            .args(["rev-parse", "--git-path", &name])
            .current_dir(&root)
            .output()
        {
            if result.status.success() {
                println!(
                    "cargo:rerun-if-changed={}",
                    String::from_utf8_lossy(&result.stdout).trim()
                );
            }
        }
    }
    let mut sources = vec![
        "Cargo.toml".into(),
        "Cargo.lock".into(),
        "build.rs".into(),
        "build_support/build_identity.rs".into(),
    ];
    files(&root, &root.join("src"), &mut sources);
    sources.sort();
    let mut hash = Sha256::new();
    for name in sources {
        let bytes = fs::read(root.join(&name)).expect("source identity bytes");
        hash.update(name.to_string_lossy().replace('\\', "/").as_bytes());
        hash.update([0]);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    let source_sha256 = format!("{:x}", hash.finalize());
    println!("cargo:rustc-env=KU_BUILD_SOURCE_SHA256={source_sha256}");
    let environment = env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    println!(
        "cargo:rustc-env=KU_BUILD_IDENTITY_SHA256={}",
        build_identity::calculate(&source_sha256, &version, &environment)
    );
    let out = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap());
    println!(
        "cargo:rustc-env=KU_BUILD_PROFILE_DIR={}",
        out.ancestors().nth(3).unwrap().display()
    );
    for key in ["TARGET", "HOST", "PROFILE"] {
        println!("cargo:rustc-env=KU_BUILD_{key}={}", env::var(key).unwrap());
    }
    println!("cargo:rustc-env=KU_BUILD_SOURCE_ROOT={}", root.display());
    for path in [
        "src",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "build_support",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
}
