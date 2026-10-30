use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Shared by the Cargo build script and its configuration-identity regression.
/// Only build inputs are selected; unrelated user environment is never exported.
pub(crate) fn calculate(
    source_sha256: &str,
    rustc_verbose: &str,
    environment: &BTreeMap<String, String>,
) -> String {
    let mut digest = Sha256::new();
    for value in [source_sha256, rustc_verbose] {
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    for (key, value) in environment {
        if matches!(
            key.as_str(),
            "HOST"
                | "TARGET"
                | "PROFILE"
                | "OPT_LEVEL"
                | "DEBUG"
                | "CARGO_CFG_TARGET_FEATURE"
                | "CARGO_CFG_PANIC"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "RUSTFLAGS"
                | "CARGO_CFG_TARGET_ARCH"
                | "CARGO_CFG_TARGET_OS"
                | "CARGO_CFG_TARGET_ENV"
        ) || key.starts_with("CARGO_FEATURE_")
            || key.starts_with("CARGO_PROFILE_")
            || matches!(key.as_str(), "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER")
        {
            for part in [key, value] {
                digest.update((part.len() as u64).to_le_bytes());
                digest.update(part.as_bytes());
            }
        }
    }
    format!("{:x}", digest.finalize())
}
