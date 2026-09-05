#[allow(dead_code)]
#[path = "support/bounded_process.rs"]
mod bounded_process;
#[path = "support/disconnected_stdout.rs"]
mod disconnected_stdout;

use bounded_process::{run_bounded, OutputLimits};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env, fs,
    io::Read,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

const CHILD_TIMEOUT: Duration = Duration::from_secs(15);
const CHILD_OUTPUT_LIMITS: OutputLimits = OutputLimits::new(8 * 1024, 12 * 1024);
const ISSUE_PROCESSES: usize = 6;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).expect("get OS randomness for test directory");
        let path = env::temp_dir().join(format!(
            "ku-registry-admin-process-{}-{}",
            std::process::id(),
            encode_hex(&random)
        ));
        fs::create_dir(&path).expect("create registry admin process test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn registry_command(credentials: &PathBuf) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ku-registry"));
    command.env("KU_REGISTRY_CREDENTIALS_FILE", credentials);
    command
}

#[test]
fn concurrent_process_issues_are_serialized_without_lost_credentials_or_plaintext() {
    let root = TestDirectory::new();
    let credentials = root.0.join("credentials.txt");
    let barrier = Arc::new(Barrier::new(ISSUE_PROCESSES));
    let mut children = Vec::new();
    for _ in 0..ISSUE_PROCESSES {
        let barrier = Arc::clone(&barrier);
        let credentials = credentials.clone();
        children.push(thread::spawn(move || {
            barrier.wait();
            let output = run_bounded(
                registry_command(&credentials).args(["token", "issue", "math"]),
                CHILD_TIMEOUT,
                CHILD_OUTPUT_LIMITS,
            )
            .expect("bounded token issue process");
            assert!(
                output.status.success(),
                "token issue failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
            let stdout = String::from_utf8(output.stdout).expect("issued token is ASCII");
            let token = stdout
                .strip_suffix('\n')
                .or_else(|| stdout.strip_suffix("\r\n"))
                .expect("issued token has exactly one output line")
                .to_string();
            assert!(!token.contains('\n'));
            assert!(token.starts_with("ku_"));
            assert_eq!(token.len(), 67);
            token
        }));
    }

    let tokens = children
        .into_iter()
        .map(|child| child.join().expect("token issue test thread"))
        .collect::<Vec<_>>();
    assert_eq!(tokens.iter().collect::<HashSet<_>>().len(), ISSUE_PROCESSES);

    let stored = fs::read(&credentials).expect("read concurrent credentials result");
    let stored_text = std::str::from_utf8(&stored).expect("credentials stay UTF-8");
    let lines = stored_text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), ISSUE_PROCESSES);
    for token in &tokens {
        assert!(
            !stored
                .windows(token.len())
                .any(|window| window == token.as_bytes()),
            "credentials file must not store plaintext tokens"
        );
        let digest = Sha256::digest(token.as_bytes());
        let expected = format!("sha256-{} math", encode_hex(&digest));
        assert!(lines.contains(&expected.as_str()));
    }

    let before_rejected_commands = fs::read(&credentials).unwrap();
    let forbidden_argument_secret = "must-not-appear-in-diagnostics";
    let rejected_argument = run_bounded(
        registry_command(&credentials).args(["token", "revoke", "math", forbidden_argument_secret]),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .expect("bounded rejected token argument process");
    assert!(!rejected_argument.status.success());
    assert!(!String::from_utf8_lossy(&rejected_argument.stdout).contains(forbidden_argument_secret));
    assert!(!String::from_utf8_lossy(&rejected_argument.stderr).contains(forbidden_argument_secret));

    let unknown_environment_secret = "unknown-environment-secret";
    let rejected_environment = run_bounded(
        registry_command(&credentials)
            .args(["token", "revoke", "math"])
            .env("KU_REGISTRY_TOKEN", unknown_environment_secret),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .expect("bounded rejected environment token process");
    assert!(!rejected_environment.status.success());
    assert!(
        !String::from_utf8_lossy(&rejected_environment.stdout).contains(unknown_environment_secret)
    );
    assert!(
        !String::from_utf8_lossy(&rejected_environment.stderr).contains(unknown_environment_secret)
    );
    assert_eq!(fs::read(&credentials).unwrap(), before_rejected_commands);

    let revoked_token = &tokens[0];
    let output = run_bounded(
        registry_command(&credentials)
            .args(["token", "revoke", "math"])
            .env("KU_REGISTRY_TOKEN", revoked_token),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .expect("bounded token revoke process");
    assert!(
        output.status.success(),
        "token revoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "revoked math\n");
    assert!(output.stderr.is_empty());
    let after_revoke = fs::read_to_string(&credentials).expect("read revoked credentials");
    assert_eq!(after_revoke.lines().count(), ISSUE_PROCESSES - 1);
    assert!(!after_revoke.contains(&encode_hex(&Sha256::digest(revoked_token.as_bytes()))));
}

struct ClosingStdoutChild(Child);

impl Drop for ClosingStdoutChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.0.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn run_with_closed_stdout(command: &mut Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(disconnected_stdout::disconnected_stdout())
        .stderr(Stdio::piped())
        .env("RUST_BACKTRACE", "0");
    let mut child = ClosingStdoutChild(
        command
            .spawn()
            .expect("start closed-output registry process"),
    );
    let stderr = child.0.stderr.take().expect("child stderr pipe");
    let deadline = Instant::now() + CHILD_TIMEOUT;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll closed-output process") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "closed-output registry process exceeded its deadline"
        );
        thread::sleep(Duration::from_millis(5));
    };
    let mut diagnostics = Vec::new();
    stderr
        .take(12 * 1024 + 1)
        .read_to_end(&mut diagnostics)
        .expect("read bounded admin diagnostics");
    assert!(diagnostics.len() <= 12 * 1024);
    Output {
        status,
        stdout: Vec::new(),
        stderr: diagnostics,
    }
}

#[test]
fn closed_stdout_reports_committed_issue_hash_and_revoke_without_panicking_or_leaking() {
    let root = TestDirectory::new();
    let credentials = root.0.join("credentials.txt");
    let issue =
        run_with_closed_stdout(registry_command(&credentials).args(["token", "issue", "math"]));
    assert!(!issue.status.success());
    let diagnostic = String::from_utf8(issue.stderr).unwrap();
    assert!(diagnostic.contains("token output failed"));
    assert!(diagnostic.contains("committed"));
    assert!(!diagnostic.contains("panicked"));
    assert!(
        !diagnostic.contains("ku_"),
        "the lost plaintext token must not be echoed to stderr"
    );
    let first = fs::read_to_string(&credentials).expect("issue committed before output failure");
    let hash = first
        .split_ascii_whitespace()
        .next()
        .expect("committed token hash");
    assert!(hash.starts_with("sha256-"));
    assert!(
        diagnostic.contains(hash),
        "the exact recovery hash must be available despite a broken stdout"
    );
    assert_eq!(first.lines().count(), 1);

    let replacement = run_bounded(
        registry_command(&credentials).args(["token", "issue", "math"]),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .expect("issue a known token for revoke testing");
    assert!(replacement.status.success());
    assert!(replacement.stderr.is_empty());
    let token = String::from_utf8(replacement.stdout)
        .unwrap()
        .trim_end()
        .to_string();
    let revoke = run_with_closed_stdout(
        registry_command(&credentials)
            .args(["token", "revoke", "math"])
            .env("KU_REGISTRY_TOKEN", &token),
    );
    assert!(!revoke.status.success());
    let diagnostic = String::from_utf8(revoke.stderr).unwrap();
    assert!(diagnostic.contains("confirmation output failed"));
    assert!(diagnostic.contains("committed"));
    assert!(!diagnostic.contains("panicked"));
    assert!(!diagnostic.contains(&token));
    assert_eq!(fs::read_to_string(&credentials).unwrap(), first);
    let retry = run_bounded(
        registry_command(&credentials)
            .args(["token", "revoke", "math"])
            .env("KU_REGISTRY_TOKEN", &token),
        CHILD_TIMEOUT,
        CHILD_OUTPUT_LIMITS,
    )
    .expect("bounded repeat of committed revoke");
    assert!(!retry.status.success());
    let diagnostic = String::from_utf8(retry.stderr).unwrap();
    assert!(diagnostic.contains("has no credential for the requested package"));
    assert!(diagnostic.contains("already have been revoked"));
    assert!(!diagnostic.contains(&token));
    assert_eq!(fs::read_to_string(&credentials).unwrap(), first);
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
