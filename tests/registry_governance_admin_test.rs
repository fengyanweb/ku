#[allow(dead_code)]
#[path = "support/bounded_process.rs"]
mod bounded_process;

use bounded_process::{run_bounded, BoundedOutput, OutputLimits};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

const CHILD_TIMEOUT: Duration = Duration::from_secs(15);
const CHILD_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(8 * 1024, 12 * 1024);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).expect("get OS randomness for test directory");
        let path = env::temp_dir().join(format!(
            "ku-registry-governance-{}-{}",
            std::process::id(),
            encode_hex(&random)
        ));
        fs::create_dir(&path).expect("create registry governance test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(credentials: &PathBuf, arguments: &[&str]) -> BoundedOutput {
    run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku-registry"))
            .env("KU_REGISTRY_CREDENTIALS_FILE", credentials)
            .args(arguments),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .expect("run bounded registry governance command")
}

fn successful(credentials: &PathBuf, arguments: &[&str]) -> BoundedOutput {
    let output = run(credentials, arguments);
    assert!(
        output.status.success(),
        "registry command {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    output
}

fn run_with_closed_stdout(credentials: &PathBuf, arguments: &[&str]) -> BoundedOutput {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ku-registry"))
        .env("KU_REGISTRY_CREDENTIALS_FILE", credentials)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn registry governance command with piped stdout");
    drop(
        child
            .stdout
            .take()
            .expect("child stdout pipe must be available"),
    );
    let mut stderr = child
        .stderr
        .take()
        .expect("child stderr pipe must be available");
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .by_ref()
            .take(CHILD_OUTPUT_LIMITS.per_stream as u64 + 1)
            .read_to_end(&mut bytes)
            .expect("read bounded registry stderr");
        bytes
    });

    let deadline = Instant::now() + CHILD_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll registry command") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let reap_deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if child
                    .try_wait()
                    .expect("reap timed-out registry command")
                    .is_some()
                {
                    break;
                }
                assert!(
                    Instant::now() < reap_deadline,
                    "registry command did not terminate after kill"
                );
                thread::sleep(Duration::from_millis(5));
            }
            panic!("registry command with closed stdout exceeded its deadline");
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stderr = stderr_reader
        .join()
        .expect("registry stderr reader thread must not panic");
    assert!(
        stderr.len() <= CHILD_OUTPUT_LIMITS.per_stream,
        "registry stderr exceeded its fixed test budget"
    );
    assert!(
        stderr.len() <= CHILD_OUTPUT_LIMITS.total,
        "registry total output exceeded its fixed test budget"
    );
    BoundedOutput {
        status,
        stdout: Vec::new(),
        stderr,
    }
}

fn diagnostic_token_hash(stderr: &[u8]) -> String {
    let diagnostic = std::str::from_utf8(stderr).expect("registry diagnostic must be UTF-8");
    let start = diagnostic
        .find("sha256-")
        .expect("diagnostic must identify the committed token hash");
    let end = start + "sha256-".len() + 64;
    let hash = diagnostic
        .get(start..end)
        .expect("diagnostic token hash must have exactly 64 hex digits");
    assert!(hash["sha256-".len()..]
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit()));
    hash.to_string()
}

fn output_token(output: BoundedOutput) -> String {
    let stdout = String::from_utf8(output.stdout).expect("developer token output is ASCII");
    let token = stdout.trim_end_matches(['\r', '\n']).to_string();
    assert!(token.starts_with("ku_"));
    assert_eq!(token.len(), 67);
    assert!(!token.contains(['\r', '\n']));
    token
}

#[test]
fn committed_mutations_survive_closed_stdout_and_orphan_hash_is_recoverable() {
    let root = TestDirectory::new();
    let credentials = root.0.join("credentials.txt");
    successful(&credentials, &["governance", "init", "alice"]);

    let failed_confirmation = run_with_closed_stdout(&credentials, &["developer", "create", "bob"]);
    assert!(!failed_confirmation.status.success());
    let diagnostic = String::from_utf8(failed_confirmation.stderr).unwrap();
    assert!(diagnostic.contains("governance mutation was committed"));
    assert!(diagnostic.contains("already-exists or already-applied"));
    assert!(!diagnostic.contains("panicked"));
    assert!(fs::read_to_string(&credentials)
        .unwrap()
        .contains("developer bob\n"));
    successful(&credentials, &["audit", "verify"]);

    let committed_after_create = fs::read(&credentials).unwrap();
    let duplicate = run(&credentials, &["developer", "create", "bob"]);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already exists"));
    assert_eq!(fs::read(&credentials).unwrap(), committed_after_create);

    let failed_token = run_with_closed_stdout(&credentials, &["developer", "token-issue", "alice"]);
    assert!(!failed_token.status.success());
    let diagnostic = String::from_utf8(failed_token.stderr.clone()).unwrap();
    assert!(diagnostic.contains("developer credential"));
    assert!(diagnostic.contains("was committed, but token output failed"));
    assert!(diagnostic.contains("developer token-revoke-hash alice"));
    assert!(diagnostic.contains("plaintext token is not available"));
    assert!(!diagnostic.contains("ku_"));
    assert!(!diagnostic.contains("panicked"));
    let hash = diagnostic_token_hash(&failed_token.stderr);
    let committed_with_orphan = fs::read_to_string(&credentials).unwrap();
    assert!(committed_with_orphan.contains(&format!("token {hash} alice all\n")));
    successful(&credentials, &["audit", "verify"]);

    let before_wrong_identity = fs::read(&credentials).unwrap();
    let wrong_identity = run(
        &credentials,
        &["developer", "token-revoke-hash", "bob", &hash],
    );
    assert!(!wrong_identity.status.success());
    assert!(String::from_utf8_lossy(&wrong_identity.stderr).contains("not active"));
    assert_eq!(fs::read(&credentials).unwrap(), before_wrong_identity);

    let recovered = successful(
        &credentials,
        &["developer", "token-revoke-hash", "alice", &hash],
    );
    assert_eq!(
        String::from_utf8(recovered.stdout).unwrap(),
        "revoked developer token hash for alice\n"
    );
    let after_recovery = fs::read_to_string(&credentials).unwrap();
    assert!(!after_recovery.contains(&format!("token {hash} alice all\n")));
    assert!(after_recovery.contains("developer-token-revoke-hash alice:sha256-"));
    successful(&credentials, &["audit", "verify"]);

    let before_bad_hash = fs::read(&credentials).unwrap();
    let malformed = run(
        &credentials,
        &[
            "developer",
            "token-revoke-hash",
            "alice",
            "sha256-not-a-token-hash",
        ],
    );
    assert!(!malformed.status.success());
    assert_eq!(fs::read(&credentials).unwrap(), before_bad_hash);
}

#[test]
fn fresh_governance_initialization_has_one_fail_closed_bootstrap_path() {
    let root = TestDirectory::new();
    let credentials = root.0.join("credentials.txt");
    successful(&credentials, &["governance", "init", "alice"]);
    successful(&credentials, &["audit", "verify"]);
    let initialized = fs::read(&credentials).unwrap();

    let duplicate = run(&credentials, &["governance", "init", "bob"]);
    assert!(!duplicate.status.success());
    assert_eq!(fs::read(&credentials).unwrap(), initialized);

    let discarded_token = output_token(successful(
        &credentials,
        &["developer", "token-issue", "alice"],
    ));
    let revoked = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku-registry"))
            .env("KU_REGISTRY_CREDENTIALS_FILE", &credentials)
            .env("KU_REGISTRY_TOKEN", &discarded_token)
            .args(["developer", "token-revoke", "alice"]),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(revoked.status.success());
    let token = output_token(successful(
        &credentials,
        &["developer", "token-issue", "alice"],
    ));
    successful(
        &credentials,
        &["package", "claim", "math", "developer:alice"],
    );
    successful(&credentials, &["audit", "verify"]);
    let committed = fs::read_to_string(&credentials).unwrap();
    assert!(committed.contains("owner math developer:alice\n"));
    assert!(!committed.contains(&token));
}

#[test]
fn governance_migration_concurrent_updates_transfer_and_audit_are_atomic() {
    let root = TestDirectory::new();
    let credentials = root.0.join("credentials.txt");
    let alice_token = "legacy-alice-token";
    let alice_hash = Sha256::digest(alice_token.as_bytes());
    let legacy_tools_token = "legacy-tools-token";
    let legacy_tools_hash = Sha256::digest(legacy_tools_token.as_bytes());
    fs::write(
        &credentials,
        format!(
            "sha256-{} math\nsha256-{} legacy_tools\n",
            encode_hex(&alice_hash),
            encode_hex(&legacy_tools_hash)
        ),
    )
    .unwrap();

    successful(&credentials, &["governance", "migrate", "alice"]);
    successful(&credentials, &["audit", "verify"]);
    let migrated = fs::read_to_string(&credentials).unwrap();
    assert!(migrated.starts_with("schema 2\n"));
    assert!(migrated.contains("developer alice\n"));
    assert!(migrated.contains("owner math developer:alice\n"));
    assert!(migrated.contains(&format!(
        "token sha256-{} alice package:math\n",
        encode_hex(&alice_hash)
    )));
    assert!(migrated.contains("owner legacy_tools developer:alice\n"));
    assert!(migrated.contains(&format!(
        "token sha256-{} alice package:legacy_tools\n",
        encode_hex(&legacy_tools_hash)
    )));
    assert!(!migrated.contains(alice_token));
    assert!(!migrated.contains(legacy_tools_token));

    let developers = ["bob", "carol", "dave", "eve"];
    let barrier = Arc::new(Barrier::new(developers.len()));
    let children = developers
        .into_iter()
        .map(|developer| {
            let barrier = Arc::clone(&barrier);
            let credentials = credentials.clone();
            thread::spawn(move || {
                barrier.wait();
                successful(&credentials, &["developer", "create", developer]);
            })
        })
        .collect::<Vec<_>>();
    for child in children {
        child.join().expect("concurrent governance command thread");
    }
    successful(&credentials, &["audit", "verify"]);
    let after_concurrency = fs::read_to_string(&credentials).unwrap();
    for developer in developers {
        assert!(after_concurrency.contains(&format!("developer {developer}\n")));
    }

    let bob_token = output_token(successful(
        &credentials,
        &["developer", "token-issue", "bob"],
    ));
    successful(&credentials, &["team", "create", "core"]);
    successful(&credentials, &["team", "member-add", "core", "bob"]);
    successful(&credentials, &["package", "claim", "tools", "team:core"]);
    successful(
        &credentials,
        &["package", "transfer", "math", "developer:bob"],
    );
    successful(&credentials, &["audit", "verify"]);

    let before_rejected_offboarding = fs::read(&credentials).unwrap();
    let rejected = run(&credentials, &["team", "member-remove", "core", "bob"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr)
        .contains("every package owner must retain at least one active publishing token"));
    assert_eq!(fs::read(&credentials).unwrap(), before_rejected_offboarding);

    let carol_token = output_token(successful(
        &credentials,
        &["developer", "token-issue", "carol"],
    ));
    successful(&credentials, &["team", "member-add", "core", "carol"]);
    successful(&credentials, &["team", "member-remove", "core", "bob"]);
    successful(&credentials, &["audit", "verify"]);

    let before_rejected_revoke = fs::read(&credentials).unwrap();
    let rejected = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku-registry"))
            .env("KU_REGISTRY_CREDENTIALS_FILE", &credentials)
            .env("KU_REGISTRY_TOKEN", &bob_token)
            .args(["developer", "token-revoke", "bob"]),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(!rejected.status.success());
    let diagnostic = String::from_utf8(rejected.stderr).unwrap();
    assert!(diagnostic.contains("last authorization for any package"));
    assert!(!diagnostic.contains(&bob_token));
    assert_eq!(fs::read(&credentials).unwrap(), before_rejected_revoke);

    let replacement = output_token(successful(
        &credentials,
        &["developer", "token-issue", "bob"],
    ));
    assert_ne!(replacement, bob_token);
    let revoked = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_ku-registry"))
            .env("KU_REGISTRY_CREDENTIALS_FILE", &credentials)
            .env("KU_REGISTRY_TOKEN", &bob_token)
            .args(["developer", "token-revoke", "bob"]),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .unwrap();
    assert!(revoked.status.success());
    assert_eq!(
        String::from_utf8(revoked.stdout).unwrap(),
        "revoked developer token for bob\n"
    );
    assert!(revoked.stderr.is_empty());

    let committed = fs::read_to_string(&credentials).unwrap();
    assert!(!committed.contains(&bob_token));
    assert!(!committed.contains(&replacement));
    assert!(!committed.contains(&carol_token));
    assert!(!committed.contains("member core bob\n"));
    assert!(committed.contains("member core carol\n"));
    assert!(committed.contains("owner math developer:bob\n"));
    assert!(committed.contains("owner tools team:core\n"));
    assert_eq!(
        committed
            .lines()
            .filter(|line| line.starts_with("audit "))
            .count(),
        20
    );
    successful(&credentials, &["audit", "verify"]);

    let tampered = committed.replacen(
        "developer-token-revoke bob",
        "developer-token-revoke eve",
        1,
    );
    assert_ne!(tampered, committed);
    fs::write(&credentials, tampered).unwrap();
    let rejected = run(&credentials, &["audit", "verify"]);
    assert!(!rejected.status.success());
    let diagnostic = String::from_utf8_lossy(&rejected.stderr);
    assert!(diagnostic.contains("registry governance audit"));
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
