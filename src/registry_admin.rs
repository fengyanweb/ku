use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::{
    error::{KuError, KuResult},
    package::{
        validate_package_name, MAX_PUBLISH_TOKEN_BYTES, MAX_REGISTRY_INDEX_VERSIONS,
        MAX_REGISTRY_LINE_BYTES, REGISTRY_TOKEN_ENV,
    },
    span::Span,
};

pub const REGISTRY_CREDENTIALS_FILE_ENV: &str = "KU_REGISTRY_CREDENTIALS_FILE";
pub const REGISTRY_USAGE: &str = "ku-registry\nku-registry governance init <developer>\nku-registry governance migrate <developer>\nku-registry developer create <developer>\nku-registry developer token-issue <developer>\nku-registry developer token-revoke <developer>\nku-registry developer token-revoke-hash <developer> <sha256-hash>\nku-registry team create <team>\nku-registry team member-add <team> <developer>\nku-registry team member-remove <team> <developer>\nku-registry package claim <package> <developer:name|team:name>\nku-registry package transfer <package> <developer:name|team:name>\nku-registry audit verify\nku-registry token issue <exact-package-name>\nku-registry token revoke <exact-package-name>\nku-registry --help";

pub(crate) const MAX_CREDENTIAL_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GOVERNANCE_RECORDS: usize = 4_096;
const MAX_GOVERNANCE_AUDITS: usize = 32_768;
const TOKEN_RANDOM_BYTES: usize = 32;
const ADMIN_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const ADMIN_LOCK_POLL: Duration = Duration::from_millis(10);
const TOKEN_GENERATION_ATTEMPTS: usize = 16;
const GOVERNANCE_SCHEMA: &str = "schema 2";
const GOVERNANCE_AUDIT_ROOT: &str = "root";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialRecord {
    pub(crate) token_hash: [u8; 32],
    pub(crate) package_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryCommand {
    Serve,
    Help,
    Issue {
        package_name: String,
    },
    Revoke {
        package_name: String,
    },
    GovernanceInit {
        developer: String,
    },
    GovernanceMigrate {
        developer: String,
    },
    DeveloperCreate {
        developer: String,
    },
    DeveloperTokenIssue {
        developer: String,
    },
    DeveloperTokenRevoke {
        developer: String,
    },
    DeveloperTokenRevokeHash {
        developer: String,
        token_hash: [u8; 32],
    },
    TeamCreate {
        team: String,
    },
    TeamMemberAdd {
        team: String,
        developer: String,
    },
    TeamMemberRemove {
        team: String,
        developer: String,
    },
    PackageClaim {
        package_name: String,
        owner: String,
    },
    PackageTransfer {
        package_name: String,
        owner: String,
    },
    AuditVerify,
}

pub fn parse_registry_command(
    arguments: impl IntoIterator<Item = OsString>,
) -> KuResult<RegistryCommand> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments.is_empty() {
        return Ok(RegistryCommand::Serve);
    }
    if arguments.as_slice() == [OsString::from("--help")] {
        return Ok(RegistryCommand::Help);
    }
    let usage = format!("usage:\n{REGISTRY_USAGE}");
    let text = arguments
        .iter()
        .map(|value| value.to_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| admin_error("invalid_registry_command", &usage))?;
    match text.as_slice() {
        [group, action, value] if group == "token" && action == "issue" => {
            validate_admin_package_name(value)?;
            Ok(RegistryCommand::Issue {
                package_name: value.clone(),
            })
        }
        [group, action, value] if group == "token" && action == "revoke" => {
            validate_admin_package_name(value)?;
            Ok(RegistryCommand::Revoke {
                package_name: value.clone(),
            })
        }
        [group, action, developer] if group == "governance" && action == "migrate" => {
            validate_governance_name(developer, "developer")?;
            Ok(RegistryCommand::GovernanceMigrate {
                developer: developer.clone(),
            })
        }
        [group, action, developer] if group == "governance" && action == "init" => {
            validate_governance_name(developer, "developer")?;
            Ok(RegistryCommand::GovernanceInit {
                developer: developer.clone(),
            })
        }
        [group, action, developer] if group == "developer" && action == "create" => {
            validate_governance_name(developer, "developer")?;
            Ok(RegistryCommand::DeveloperCreate {
                developer: developer.clone(),
            })
        }
        [group, action, developer] if group == "developer" && action == "token-issue" => {
            validate_governance_name(developer, "developer")?;
            Ok(RegistryCommand::DeveloperTokenIssue {
                developer: developer.clone(),
            })
        }
        [group, action, developer] if group == "developer" && action == "token-revoke" => {
            validate_governance_name(developer, "developer")?;
            Ok(RegistryCommand::DeveloperTokenRevoke {
                developer: developer.clone(),
            })
        }
        [group, action, developer, token_hash]
            if group == "developer" && action == "token-revoke-hash" =>
        {
            validate_governance_name(developer, "developer")?;
            Ok(RegistryCommand::DeveloperTokenRevokeHash {
                developer: developer.clone(),
                token_hash: parse_admin_token_hash(token_hash)?,
            })
        }
        [group, action, team] if group == "team" && action == "create" => {
            validate_governance_name(team, "team")?;
            Ok(RegistryCommand::TeamCreate { team: team.clone() })
        }
        [group, action, team, developer] if group == "team" && action == "member-add" => {
            validate_governance_name(team, "team")?;
            validate_governance_name(developer, "developer")?;
            Ok(RegistryCommand::TeamMemberAdd {
                team: team.clone(),
                developer: developer.clone(),
            })
        }
        [group, action, team, developer] if group == "team" && action == "member-remove" => {
            validate_governance_name(team, "team")?;
            validate_governance_name(developer, "developer")?;
            Ok(RegistryCommand::TeamMemberRemove {
                team: team.clone(),
                developer: developer.clone(),
            })
        }
        [group, action, package_name, owner]
            if group == "package" && (action == "claim" || action == "transfer") =>
        {
            validate_admin_package_name(package_name)?;
            parse_owner(owner)?;
            if action == "claim" {
                Ok(RegistryCommand::PackageClaim {
                    package_name: package_name.clone(),
                    owner: owner.clone(),
                })
            } else {
                Ok(RegistryCommand::PackageTransfer {
                    package_name: package_name.clone(),
                    owner: owner.clone(),
                })
            }
        }
        [group, action] if group == "audit" && action == "verify" => {
            Ok(RegistryCommand::AuditVerify)
        }
        _ => Err(admin_error("invalid_registry_command", usage)),
    }
}

pub fn issue_token_from_env(package_name: &str) -> KuResult<String> {
    let credentials_path = required_credentials_path()?;
    issue_token(&credentials_path, package_name, ADMIN_LOCK_TIMEOUT)
}

pub fn revoke_token_from_env(package_name: &str) -> KuResult<()> {
    let credentials_path = required_credentials_path()?;
    let token = env::var(REGISTRY_TOKEN_ENV).map_err(|_| {
        admin_error(
            "missing_registry_token",
            format!("required registry environment variable {REGISTRY_TOKEN_ENV} is not set"),
        )
    })?;
    validate_presented_token(&token)?;
    revoke_token(
        &credentials_path,
        package_name,
        token.as_bytes(),
        ADMIN_LOCK_TIMEOUT,
    )
}

pub fn initialize_governance_from_env(developer: &str) -> KuResult<()> {
    validate_governance_name(developer, "developer")?;
    let path = normalize_credentials_path(&required_credentials_path()?)?;
    let deadline = absolute_deadline(ADMIN_LOCK_TIMEOUT)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    if first_registry_record(&original)?.is_some() {
        return Err(admin_error(
            "registry_governance_already_initialized",
            "registry credentials are not empty; migrate a legacy file or continue using schema 2",
        ));
    }
    let mut state = GovernanceState::default();
    state.developers.insert(developer.to_string());
    append_governance_audit(&mut state, "init", developer);
    let replacement = serialize_governance_state(&state)?;
    write_credentials_atomically(&path, &replacement)
}

pub fn migrate_governance_from_env(developer: &str) -> KuResult<()> {
    validate_governance_name(developer, "developer")?;
    let path = normalize_credentials_path(&required_credentials_path()?)?;
    let deadline = absolute_deadline(ADMIN_LOCK_TIMEOUT)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    if first_registry_record(&original)? == Some(GOVERNANCE_SCHEMA) {
        return Err(admin_error(
            "registry_governance_already_migrated",
            "registry credentials already use governance schema 2",
        ));
    }
    let legacy = parse_credential_records(&original)?;
    if legacy.is_empty() {
        return Err(admin_error(
            "invalid_registry_credentials",
            "legacy credentials must contain at least one authorization before migration",
        ));
    }
    let state = migrate_legacy_credentials(legacy, developer);
    let replacement = serialize_governance_state(&state)?;
    write_credentials_atomically(&path, &replacement)
}

pub fn create_developer_from_env(developer: &str) -> KuResult<()> {
    validate_governance_name(developer, "developer")?;
    mutate_governance_from_env("developer-create", developer, |state| {
        if !state.developers.insert(developer.to_string()) {
            return Err(admin_error(
                "registry_developer_exists",
                "registry developer already exists",
            ));
        }
        Ok(())
    })
}

pub fn create_team_from_env(team: &str) -> KuResult<()> {
    validate_governance_name(team, "team")?;
    mutate_governance_from_env("team-create", team, |state| {
        if !state.teams.insert(team.to_string()) {
            return Err(admin_error(
                "registry_team_exists",
                "registry team already exists",
            ));
        }
        Ok(())
    })
}

pub fn add_team_member_from_env(team: &str, developer: &str) -> KuResult<()> {
    validate_governance_name(team, "team")?;
    validate_governance_name(developer, "developer")?;
    let subject = format!("{team}:{developer}");
    mutate_governance_from_env("team-member-add", &subject, |state| {
        if !state.teams.contains(team) || !state.developers.contains(developer) {
            return Err(admin_error(
                "registry_governance_not_found",
                "team membership requires an existing team and developer",
            ));
        }
        if !state
            .members
            .insert((team.to_string(), developer.to_string()))
        {
            return Err(admin_error(
                "registry_team_member_exists",
                "registry team membership already exists",
            ));
        }
        Ok(())
    })
}

pub fn remove_team_member_from_env(team: &str, developer: &str) -> KuResult<()> {
    validate_governance_name(team, "team")?;
    validate_governance_name(developer, "developer")?;
    let subject = format!("{team}:{developer}");
    mutate_governance_from_env("team-member-remove", &subject, |state| {
        if !state
            .members
            .remove(&(team.to_string(), developer.to_string()))
        {
            return Err(admin_error(
                "registry_team_member_not_found",
                "registry team membership does not exist",
            ));
        }
        Ok(())
    })
}

pub fn claim_package_from_env(package_name: &str, owner: &str) -> KuResult<()> {
    change_package_owner_from_env(package_name, owner, false)
}

pub fn transfer_package_from_env(package_name: &str, owner: &str) -> KuResult<()> {
    change_package_owner_from_env(package_name, owner, true)
}

pub fn issue_developer_token_from_env(developer: &str) -> KuResult<String> {
    validate_governance_name(developer, "developer")?;
    let path = normalize_credentials_path(&required_credentials_path()?)?;
    let deadline = absolute_deadline(ADMIN_LOCK_TIMEOUT)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    let mut state = require_governance_state(&original)?;
    if !state.developers.contains(developer) {
        return Err(admin_error(
            "registry_developer_not_found",
            "registry developer does not exist",
        ));
    }
    for _ in 0..TOKEN_GENERATION_ATTEMPTS {
        let token = generate_token()?;
        let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if state
            .tokens
            .iter()
            .any(|existing| existing.token_hash == token_hash)
        {
            continue;
        }
        let issued = GovernanceToken {
            token_hash,
            developer: developer.to_string(),
            scope: GovernanceTokenScope::All,
        };
        state.tokens.insert(issued.clone());
        let audit_subject = token_audit_subject(&issued);
        append_governance_audit(&mut state, "developer-token-issue", &audit_subject);
        let replacement = serialize_governance_state(&state)?;
        write_credentials_atomically(&path, &replacement).map_err(|error| {
            annotate_committed_credential_error(
                error,
                "issued",
                "developer",
                developer,
                &token_hash,
            )
        })?;
        return Ok(token);
    }
    Err(admin_error(
        "registry_random_failed",
        "failed to generate a unique registry developer token",
    ))
}

pub fn revoke_developer_token_from_env(developer: &str) -> KuResult<()> {
    validate_governance_name(developer, "developer")?;
    let token = env::var(REGISTRY_TOKEN_ENV).map_err(|_| {
        admin_error(
            "missing_registry_token",
            format!("required registry environment variable {REGISTRY_TOKEN_ENV} is not set"),
        )
    })?;
    validate_presented_token(&token)?;
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let audit_subject = format!("{developer}:sha256-{}", encode_hex(&token_hash));
    revoke_developer_token_hash_from_env_inner(
        developer,
        token_hash,
        "developer-token-revoke",
        &audit_subject,
    )
}

pub fn revoke_developer_token_hash_from_env(developer: &str, token_hash: [u8; 32]) -> KuResult<()> {
    validate_governance_name(developer, "developer")?;
    let hash_text = format!("sha256-{}", encode_hex(&token_hash));
    let subject = format!("{developer}:{hash_text}");
    revoke_developer_token_hash_from_env_inner(
        developer,
        token_hash,
        "developer-token-revoke-hash",
        &subject,
    )
}

fn revoke_developer_token_hash_from_env_inner(
    developer: &str,
    token_hash: [u8; 32],
    audit_action: &str,
    audit_subject: &str,
) -> KuResult<()> {
    validate_governance_name(developer, "developer")?;
    validate_governance_name(audit_action, "audit action")?;
    validate_audit_subject(audit_subject)?;
    let path = normalize_credentials_path(&required_credentials_path()?)?;
    let deadline = absolute_deadline(ADMIN_LOCK_TIMEOUT)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    let mut state = require_governance_state(&original)?;
    let before = state.tokens.len();
    state
        .tokens
        .retain(|token| token.token_hash != token_hash || token.developer != developer);
    if state.tokens.len() == before {
        return Err(admin_error(
            "registry_credential_not_found",
            "the supplied token is not active for that developer",
        ));
    }
    if !all_packages_remain_authorized(&state) {
        return Err(admin_error(
            "registry_last_credential",
            "cannot revoke a developer token while it is the last authorization for any package",
        ));
    }
    append_governance_audit(&mut state, audit_action, audit_subject);
    let replacement = serialize_governance_state(&state)?;
    write_credentials_atomically(&path, &replacement).map_err(|error| {
        annotate_committed_credential_error(error, "revoked", "developer", developer, &token_hash)
    })
}

pub fn verify_governance_from_env() -> KuResult<()> {
    let path = normalize_credentials_path(&required_credentials_path()?)?;
    let bytes = read_credentials_if_present(&path)?;
    require_governance_state(&bytes).map(|_| ())
}

pub fn write_issued_token(token: &str, output: &mut impl Write) -> KuResult<()> {
    validate_presented_token(token)?;
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    write_token_line(token, output).map_err(|err| {
        admin_error(
            "registry_token_output_failed",
            format!(
                "issued credential sha256-{} was committed, but token output failed ({:?}); inspect and revoke this exact hash from the credentials file before retrying; the plaintext token is not available in diagnostics",
                encode_hex(&token_hash), err.kind()
            ),
        )
    })
}

pub fn write_developer_issued_token(
    developer: &str,
    token: &str,
    output: &mut impl Write,
) -> KuResult<()> {
    validate_governance_name(developer, "developer")?;
    validate_presented_token(token)?;
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let hash_text = format!("sha256-{}", encode_hex(&token_hash));
    write_token_line(token, output).map_err(|err| {
        admin_error(
            "registry_token_output_failed",
            format!(
                "issued developer credential {hash_text} was committed, but token output failed ({:?}); run 'ku-registry developer token-revoke-hash {developer} {hash_text}' before retrying; the plaintext token is not available in diagnostics",
                err.kind()
            ),
        )
    })
}

fn write_token_line(token: &str, output: &mut impl Write) -> io::Result<()> {
    writeln!(output, "{token}").and_then(|()| output.flush())
}

pub fn write_governance_confirmation(confirmation: &str, output: &mut impl Write) -> KuResult<()> {
    writeln!(output, "{confirmation}")
        .and_then(|()| output.flush())
        .map_err(|err| {
            admin_error(
                "registry_governance_output_failed",
                format!(
                    "registry governance mutation was committed, but confirmation output failed ({:?}); the state is already effective on disk and a retry may return an already-exists or already-applied error; inspect the audit chain before retrying",
                    err.kind()
                ),
            )
        })
}

pub fn write_audit_verification_confirmation(output: &mut impl Write) -> KuResult<()> {
    writeln!(output, "registry governance audit verified")
        .and_then(|()| output.flush())
        .map_err(|err| {
            admin_error(
                "registry_audit_output_failed",
                format!(
                    "registry governance audit verification succeeded, but output failed ({:?})",
                    err.kind()
                ),
            )
        })
}

pub fn write_revocation_confirmation(package_name: &str, output: &mut impl Write) -> KuResult<()> {
    validate_admin_package_name(package_name)?;
    writeln!(output, "revoked {package_name}")
        .and_then(|()| output.flush())
        .map_err(|err| {
            admin_error(
                "registry_revocation_output_failed",
                format!(
                    "registry credential revocation was committed, but confirmation output failed ({:?}); a retry can report that the credential was already revoked",
                    err.kind()
                ),
            )
        })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GovernanceOwner {
    Developer(String),
    Team(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GovernanceTokenScope {
    All,
    Package(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GovernanceToken {
    token_hash: [u8; 32],
    developer: String,
    scope: GovernanceTokenScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GovernanceAudit {
    sequence: u64,
    previous_hash: String,
    event_hash: String,
    before_transition_hash: String,
    transition_hash: String,
    action: String,
    subject: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GovernanceAuditPhase {
    Empty,
    Migrating,
    Normal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GovernanceState {
    developers: BTreeSet<String>,
    teams: BTreeSet<String>,
    members: BTreeSet<(String, String)>,
    owners: BTreeMap<String, GovernanceOwner>,
    tokens: BTreeSet<GovernanceToken>,
    audits: Vec<GovernanceAudit>,
}

fn migrate_legacy_credentials(legacy: Vec<CredentialRecord>, developer: &str) -> GovernanceState {
    let mut state = GovernanceState::default();
    state.developers.insert(developer.to_string());
    append_governance_audit(&mut state, "migration-init", developer);
    for record in legacy {
        let package_name = record.package_name;
        let token = GovernanceToken {
            token_hash: record.token_hash,
            developer: developer.to_string(),
            scope: GovernanceTokenScope::Package(package_name.clone()),
        };
        if state.tokens.insert(token.clone()) {
            let subject = token_audit_subject(&token);
            append_governance_audit(&mut state, "migration-token-import", &subject);
        }
        if !state.owners.contains_key(&package_name) {
            state.owners.insert(
                package_name.clone(),
                GovernanceOwner::Developer(developer.to_string()),
            );
            let subject = format!("{package_name}:developer:{developer}");
            append_governance_audit(&mut state, "package-claim", &subject);
        }
    }
    append_governance_audit(&mut state, "migration-complete", developer);
    state
}

fn validate_governance_name(value: &str, kind: &str) -> KuResult<()> {
    validate_package_name(value, Span::default()).map_err(|_| {
        admin_error(
            "invalid_registry_governance_name",
            format!("registry {kind} name must use the package-name character set"),
        )
    })
}

fn parse_owner(value: &str) -> KuResult<GovernanceOwner> {
    let (kind, name) = value.split_once(':').ok_or_else(|| {
        admin_error(
            "invalid_registry_owner",
            "registry owner must be exactly developer:<name> or team:<name>",
        )
    })?;
    validate_governance_name(name, kind)?;
    match kind {
        "developer" => Ok(GovernanceOwner::Developer(name.to_string())),
        "team" => Ok(GovernanceOwner::Team(name.to_string())),
        _ => Err(admin_error(
            "invalid_registry_owner",
            "registry owner must be exactly developer:<name> or team:<name>",
        )),
    }
}

fn owner_text(owner: &GovernanceOwner) -> String {
    match owner {
        GovernanceOwner::Developer(name) => format!("developer:{name}"),
        GovernanceOwner::Team(name) => format!("team:{name}"),
    }
}

fn governance_audit_hash(
    sequence: u64,
    previous_hash: &str,
    before_transition_hash: &str,
    transition_hash: &str,
    action: &str,
    subject: &str,
) -> String {
    let canonical = format!(
        "{sequence}\n{previous_hash}\n{before_transition_hash}\n{transition_hash}\n{action}\n{subject}\n"
    );
    encode_hex(&Sha256::digest(canonical.as_bytes()))
}

fn governance_transition_root() -> String {
    encode_hex(&Sha256::digest(
        b"ku-registry-governance-transition-root-v1",
    ))
}

fn governance_transition_hash(previous: &str, action: &str, subject: &str) -> String {
    let canonical = format!("{previous}\n{action}\n{subject}\n");
    encode_hex(&Sha256::digest(canonical.as_bytes()))
}

fn append_governance_audit(state: &mut GovernanceState, action: &str, subject: &str) {
    let sequence = state.audits.len() as u64 + 1;
    let previous_hash = state
        .audits
        .last()
        .map(|event| event.event_hash.clone())
        .unwrap_or_else(|| GOVERNANCE_AUDIT_ROOT.to_string());
    let before_transition_hash = state
        .audits
        .last()
        .map(|event| event.transition_hash.clone())
        .unwrap_or_else(governance_transition_root);
    let transition_hash = governance_transition_hash(&before_transition_hash, action, subject);
    let event_hash = governance_audit_hash(
        sequence,
        &previous_hash,
        &before_transition_hash,
        &transition_hash,
        action,
        subject,
    );
    state.audits.push(GovernanceAudit {
        sequence,
        previous_hash,
        event_hash,
        before_transition_hash,
        transition_hash,
        action: action.to_string(),
        subject: subject.to_string(),
    });
}

fn invalid_audit_transition(message: impl Into<String>) -> KuError {
    admin_error(
        "invalid_registry_audit",
        format!(
            "registry governance audit transition is invalid: {}",
            message.into()
        ),
    )
}

fn parse_audit_owner_subject(subject: &str) -> KuResult<(&str, GovernanceOwner)> {
    let mut fields = subject.split(':');
    let (Some(package), Some(kind), Some(name), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(invalid_audit_transition(
            "package audit subject must be '<package>:<developer|team>:<name>'",
        ));
    };
    validate_admin_package_name(package)?;
    Ok((package, parse_owner(&format!("{kind}:{name}"))?))
}

fn parse_audit_member_subject(subject: &str) -> KuResult<(&str, &str)> {
    let mut fields = subject.split(':');
    let (Some(team), Some(developer), None) = (fields.next(), fields.next(), fields.next()) else {
        return Err(invalid_audit_transition(
            "team membership audit subject must be '<team>:<developer>'",
        ));
    };
    validate_governance_name(team, "team")?;
    validate_governance_name(developer, "developer")?;
    Ok((team, developer))
}

fn token_audit_subject(token: &GovernanceToken) -> String {
    let scope = match &token.scope {
        GovernanceTokenScope::All => "all".to_string(),
        GovernanceTokenScope::Package(package) => format!("package:{package}"),
    };
    format!(
        "{}:sha256-{}:{scope}",
        token.developer,
        encode_hex(&token.token_hash)
    )
}

fn parse_audit_token_subject(subject: &str) -> KuResult<GovernanceToken> {
    let fields = subject.split(':').collect::<Vec<_>>();
    let (developer, hash, scope) = match fields.as_slice() {
        [developer, hash, "all"] => (*developer, *hash, GovernanceTokenScope::All),
        [developer, hash, "package", package] => {
            validate_admin_package_name(package)?;
            (
                *developer,
                *hash,
                GovernanceTokenScope::Package((*package).to_string()),
            )
        }
        _ => {
            return Err(invalid_audit_transition(
                "token issue audit subject has an invalid shape",
            ));
        }
    };
    validate_governance_name(developer, "developer")?;
    let token = GovernanceToken {
        token_hash: parse_admin_token_hash(hash)?,
        developer: developer.to_string(),
        scope,
    };
    if token_audit_subject(&token) != subject {
        return Err(invalid_audit_transition(
            "token issue audit subject is not canonically encoded",
        ));
    }
    Ok(token)
}

fn parse_audit_revoke_subject(subject: &str) -> KuResult<(&str, [u8; 32])> {
    let mut fields = subject.split(':');
    let (Some(developer), Some(hash), None) = (fields.next(), fields.next(), fields.next()) else {
        return Err(invalid_audit_transition(
            "token revoke audit subject must be '<developer>:<sha256-hash>'",
        ));
    };
    validate_governance_name(developer, "developer")?;
    let token_hash = parse_admin_token_hash(hash)?;
    if format!("{developer}:sha256-{}", encode_hex(&token_hash)) != subject {
        return Err(invalid_audit_transition(
            "token revoke audit subject is not canonically encoded",
        ));
    }
    Ok((developer, token_hash))
}

fn insert_replayed_governance_token(state: &mut GovernanceState, token: GovernanceToken) -> bool {
    if !state.developers.contains(&token.developer)
        || state.tokens.iter().any(|existing| {
            existing.token_hash == token.token_hash
                && (existing.developer != token.developer
                    || matches!(existing.scope, GovernanceTokenScope::All)
                    || matches!(token.scope, GovernanceTokenScope::All))
        })
    {
        return false;
    }
    state.tokens.insert(token)
}

fn apply_governance_audit_event(
    state: &mut GovernanceState,
    phase: &mut GovernanceAuditPhase,
    action: &str,
    subject: &str,
) -> KuResult<()> {
    match action {
        "init" => {
            validate_governance_name(subject, "developer")?;
            if *phase != GovernanceAuditPhase::Empty
                || state != &GovernanceState::default()
                || !state.developers.insert(subject.to_string())
            {
                return Err(invalid_audit_transition(
                    "init must be the first event and create one developer",
                ));
            }
            *phase = GovernanceAuditPhase::Normal;
        }
        "migration-init" => {
            validate_governance_name(subject, "developer")?;
            if *phase != GovernanceAuditPhase::Empty
                || state != &GovernanceState::default()
                || !state.developers.insert(subject.to_string())
            {
                return Err(invalid_audit_transition(
                    "migration-init must be the first event and create one developer",
                ));
            }
            *phase = GovernanceAuditPhase::Migrating;
        }
        "developer-create" => {
            validate_governance_name(subject, "developer")?;
            if *phase != GovernanceAuditPhase::Normal
                || !state.developers.insert(subject.to_string())
            {
                return Err(invalid_audit_transition(
                    "developer-create must add a new developer after bootstrap",
                ));
            }
        }
        "team-create" => {
            validate_governance_name(subject, "team")?;
            if *phase != GovernanceAuditPhase::Normal || !state.teams.insert(subject.to_string()) {
                return Err(invalid_audit_transition("team-create must add a new team"));
            }
        }
        "team-member-add" => {
            let (team, developer) = parse_audit_member_subject(subject)?;
            if *phase != GovernanceAuditPhase::Normal
                || !state.teams.contains(team)
                || !state.developers.contains(developer)
                || !state
                    .members
                    .insert((team.to_string(), developer.to_string()))
            {
                return Err(invalid_audit_transition(
                    "team-member-add must add an existing developer to an existing team",
                ));
            }
        }
        "team-member-remove" => {
            let (team, developer) = parse_audit_member_subject(subject)?;
            if *phase != GovernanceAuditPhase::Normal
                || !state
                    .members
                    .remove(&(team.to_string(), developer.to_string()))
                || !all_packages_remain_authorized(state)
            {
                return Err(invalid_audit_transition(
                    "team-member-remove must remove a membership without orphaning a package",
                ));
            }
        }
        "package-claim" | "package-transfer" => {
            let (package, owner) = parse_audit_owner_subject(subject)?;
            if *phase == GovernanceAuditPhase::Empty
                || (*phase == GovernanceAuditPhase::Migrating && action != "package-claim")
            {
                return Err(invalid_audit_transition(
                    "package ownership actions must follow bootstrap and migration cannot transfer",
                ));
            }
            let owner_exists = match &owner {
                GovernanceOwner::Developer(name) => state.developers.contains(name),
                GovernanceOwner::Team(name) => state.teams.contains(name),
            };
            if !owner_exists || !owner_has_active_token(state, package, &owner) {
                return Err(invalid_audit_transition(
                    "package ownership audit requires an existing authorized owner",
                ));
            }
            match (action, state.owners.get(package)) {
                ("package-claim", None) => {}
                ("package-transfer", Some(existing)) if existing != &owner => {}
                _ => {
                    return Err(invalid_audit_transition(
                        "package ownership audit does not describe a valid state change",
                    ));
                }
            }
            state.owners.insert(package.to_string(), owner);
        }
        "developer-token-issue" => {
            let token = parse_audit_token_subject(subject)?;
            if *phase != GovernanceAuditPhase::Normal
                || !matches!(token.scope, GovernanceTokenScope::All)
                || !insert_replayed_governance_token(state, token)
            {
                return Err(invalid_audit_transition(
                    "developer-token-issue must add one nonconflicting all-scoped token after bootstrap",
                ));
            }
        }
        "migration-token-import" => {
            let token = parse_audit_token_subject(subject)?;
            if *phase != GovernanceAuditPhase::Migrating
                || !matches!(token.scope, GovernanceTokenScope::Package(_))
                || !insert_replayed_governance_token(state, token)
            {
                return Err(invalid_audit_transition(
                    "migration-token-import must add one nonconflicting package-scoped token during migration",
                ));
            }
        }
        "migration-complete" => {
            validate_governance_name(subject, "developer")?;
            if *phase != GovernanceAuditPhase::Migrating
                || state.developers.len() != 1
                || !state.developers.contains(subject)
                || state.tokens.is_empty()
                || state.owners.is_empty()
                || state
                    .tokens
                    .iter()
                    .any(|token| !matches!(token.scope, GovernanceTokenScope::Package(_)))
                || !all_packages_remain_authorized(state)
            {
                return Err(invalid_audit_transition(
                    "migration-complete must close one nonempty exact-scope legacy import",
                ));
            }
            *phase = GovernanceAuditPhase::Normal;
        }
        "developer-token-revoke" | "developer-token-revoke-hash" => {
            let (developer, token_hash) = parse_audit_revoke_subject(subject)?;
            let before = state.tokens.len();
            state
                .tokens
                .retain(|token| token.token_hash != token_hash || token.developer != developer);
            if *phase != GovernanceAuditPhase::Normal
                || state.tokens.len() == before
                || !all_packages_remain_authorized(state)
            {
                return Err(invalid_audit_transition(
                    "developer token revoke must remove an active token without orphaning a package",
                ));
            }
        }
        _ => {
            return Err(invalid_audit_transition(format!(
                "unknown registry governance audit action '{action}'"
            )));
        }
    }
    Ok(())
}

fn validate_governance_state(state: &GovernanceState) -> KuResult<()> {
    if state.developers.len() > MAX_GOVERNANCE_RECORDS
        || state.teams.len() > MAX_GOVERNANCE_RECORDS
        || state.members.len() > MAX_GOVERNANCE_RECORDS
        || state.owners.len() > MAX_GOVERNANCE_RECORDS
        || state.tokens.len() > MAX_GOVERNANCE_RECORDS
        || state.audits.len() > MAX_GOVERNANCE_AUDITS
    {
        return Err(admin_error(
            "invalid_registry_governance",
            "registry governance file exceeds its bounded record limit",
        ));
    }
    for (team, developer) in &state.members {
        if !state.teams.contains(team) || !state.developers.contains(developer) {
            return Err(admin_error(
                "invalid_registry_governance",
                "team membership references an unknown team or developer",
            ));
        }
    }
    for owner in state.owners.values() {
        let present = match owner {
            GovernanceOwner::Developer(name) => state.developers.contains(name),
            GovernanceOwner::Team(name) => state.teams.contains(name),
        };
        if !present {
            return Err(admin_error(
                "invalid_registry_governance",
                "package ownership references an unknown developer or team",
            ));
        }
    }
    let mut token_identities = BTreeMap::new();
    let mut all_scoped_hashes = BTreeSet::new();
    for token in &state.tokens {
        if !state.developers.contains(&token.developer) {
            return Err(admin_error(
                "invalid_registry_governance",
                "developer token references an unknown developer",
            ));
        }
        if let Some(existing) = token_identities.insert(token.token_hash, &token.developer) {
            if existing != &token.developer {
                return Err(admin_error(
                    "invalid_registry_governance",
                    "one developer token hash cannot belong to multiple developers",
                ));
            }
        }
        match &token.scope {
            GovernanceTokenScope::All => {
                if !all_scoped_hashes.insert(token.token_hash) {
                    return Err(admin_error(
                        "invalid_registry_governance",
                        "developer token contains redundant scopes",
                    ));
                }
            }
            GovernanceTokenScope::Package(package_name) => {
                validate_admin_package_name(package_name)?;
                if !state.owners.contains_key(package_name) {
                    return Err(admin_error(
                        "invalid_registry_governance",
                        "package-scoped developer token references an unknown package",
                    ));
                }
            }
        }
    }
    for token in &state.tokens {
        if matches!(token.scope, GovernanceTokenScope::Package(_))
            && all_scoped_hashes.contains(&token.token_hash)
        {
            return Err(admin_error(
                "invalid_registry_governance",
                "developer token cannot combine all and package-specific scopes",
            ));
        }
    }
    if state.audits.is_empty() {
        return Err(admin_error(
            "invalid_registry_audit",
            "registry governance audit chain must contain at least one event",
        ));
    }
    let effective_credentials = governance_credentials(state)?;
    if effective_credentials
        .iter()
        .map(|credential| credential.package_name.as_str())
        .collect::<BTreeSet<_>>()
        .len()
        != state.owners.len()
    {
        return Err(admin_error(
            "invalid_registry_governance",
            "every package owner must have at least one active publishing token",
        ));
    }
    let mut previous = GOVERNANCE_AUDIT_ROOT.to_string();
    let mut transition = governance_transition_root();
    let mut phase = GovernanceAuditPhase::Empty;
    let mut replayed = GovernanceState::default();
    for (index, event) in state.audits.iter().enumerate() {
        let expected_sequence = index as u64 + 1;
        if event.before_transition_hash != transition {
            return Err(invalid_audit_transition(
                "registry governance audit transition hash is not continuous",
            ));
        }
        apply_governance_audit_event(&mut replayed, &mut phase, &event.action, &event.subject)?;
        let expected_transition =
            governance_transition_hash(&transition, &event.action, &event.subject);
        if event.transition_hash != expected_transition
            || event.transition_hash == event.before_transition_hash
        {
            return Err(invalid_audit_transition(
                "registry governance audit event has an invalid transition hash",
            ));
        }
        let expected_hash = governance_audit_hash(
            expected_sequence,
            &previous,
            &event.before_transition_hash,
            &event.transition_hash,
            &event.action,
            &event.subject,
        );
        if event.sequence != expected_sequence
            || event.previous_hash != previous
            || event.event_hash != expected_hash
        {
            return Err(admin_error(
                "invalid_registry_audit",
                "registry governance audit hash chain is invalid",
            ));
        }
        previous.clone_from(&event.event_hash);
        transition.clone_from(&event.transition_hash);
    }
    if phase != GovernanceAuditPhase::Normal {
        return Err(invalid_audit_transition(
            "registry governance audit bootstrap or migration is incomplete",
        ));
    }
    if !governance_state_data_equal(&replayed, state) {
        return Err(admin_error(
            "invalid_registry_audit",
            "registry governance audit replay does not equal the current state",
        ));
    }
    Ok(())
}

fn governance_state_data_equal(left: &GovernanceState, right: &GovernanceState) -> bool {
    left.developers == right.developers
        && left.teams == right.teams
        && left.members == right.members
        && left.owners == right.owners
        && left.tokens == right.tokens
}

fn ensure_governance_parse_capacity(
    current_len: usize,
    limit: usize,
    record_kind: &str,
    line_number: usize,
) -> KuResult<()> {
    if current_len >= limit {
        return Err(admin_error(
            "invalid_registry_governance",
            format!(
                "registry governance {record_kind} record limit {limit} exceeded at line {line_number}"
            ),
        ));
    }
    Ok(())
}

fn parse_governance_state(bytes: &[u8]) -> KuResult<GovernanceState> {
    let source = std::str::from_utf8(bytes).map_err(|_| {
        admin_error(
            "invalid_registry_governance",
            "registry governance file must be valid UTF-8",
        )
    })?;
    let mut state = GovernanceState::default();
    let mut saw_schema = false;
    for (index, raw) in source.lines().enumerate() {
        if raw.len() > MAX_REGISTRY_LINE_BYTES {
            return Err(admin_error(
                "invalid_registry_governance",
                format!("registry governance line {} is too long", index + 1),
            ));
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !saw_schema {
            if line != GOVERNANCE_SCHEMA {
                return Err(admin_error(
                    "invalid_registry_governance",
                    format!(
                        "registry governance line {} must be '{GOVERNANCE_SCHEMA}'",
                        index + 1
                    ),
                ));
            }
            saw_schema = true;
            continue;
        }
        let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
        match fields.as_slice() {
            ["developer", name] => {
                ensure_governance_parse_capacity(
                    state.developers.len(),
                    MAX_GOVERNANCE_RECORDS,
                    "developer",
                    index + 1,
                )?;
                validate_governance_name(name, "developer")?;
                if !state.developers.insert((*name).to_string()) {
                    return Err(admin_error(
                        "invalid_registry_governance",
                        "duplicate developer",
                    ));
                }
            }
            ["team", name] => {
                ensure_governance_parse_capacity(
                    state.teams.len(),
                    MAX_GOVERNANCE_RECORDS,
                    "team",
                    index + 1,
                )?;
                validate_governance_name(name, "team")?;
                if !state.teams.insert((*name).to_string()) {
                    return Err(admin_error("invalid_registry_governance", "duplicate team"));
                }
            }
            ["member", team, developer] => {
                ensure_governance_parse_capacity(
                    state.members.len(),
                    MAX_GOVERNANCE_RECORDS,
                    "member",
                    index + 1,
                )?;
                validate_governance_name(team, "team")?;
                validate_governance_name(developer, "developer")?;
                if !state
                    .members
                    .insert(((*team).to_string(), (*developer).to_string()))
                {
                    return Err(admin_error(
                        "invalid_registry_governance",
                        "duplicate team membership",
                    ));
                }
            }
            ["owner", package, owner] => {
                ensure_governance_parse_capacity(
                    state.owners.len(),
                    MAX_GOVERNANCE_RECORDS,
                    "owner",
                    index + 1,
                )?;
                validate_admin_package_name(package)?;
                if state
                    .owners
                    .insert((*package).to_string(), parse_owner(owner)?)
                    .is_some()
                {
                    return Err(admin_error(
                        "invalid_registry_governance",
                        "duplicate package owner",
                    ));
                }
            }
            ["token", hash, developer, scope] => {
                ensure_governance_parse_capacity(
                    state.tokens.len(),
                    MAX_GOVERNANCE_RECORDS,
                    "token",
                    index + 1,
                )?;
                let hash = hash
                    .strip_prefix("sha256-")
                    .ok_or_else(|| invalid_hash_error(index + 1))?;
                if !is_canonical_sha256_hex(hash) {
                    return Err(invalid_hash_error(index + 1));
                }
                let token_hash =
                    decode_hex_array::<32>(hash).map_err(|_| invalid_hash_error(index + 1))?;
                validate_governance_name(developer, "developer")?;
                let scope = if scope == &"all" {
                    GovernanceTokenScope::All
                } else if let Some(package_name) = scope.strip_prefix("package:") {
                    validate_admin_package_name(package_name)?;
                    GovernanceTokenScope::Package(package_name.to_string())
                } else {
                    return Err(admin_error(
                        "invalid_registry_governance",
                        "developer token scope must be 'all' or 'package:<name>'",
                    ));
                };
                if !state.tokens.insert(GovernanceToken {
                    token_hash,
                    developer: (*developer).to_string(),
                    scope,
                }) {
                    return Err(admin_error(
                        "invalid_registry_governance",
                        "duplicate developer token",
                    ));
                }
            }
            ["audit", sequence, previous_hash, event_hash, before_transition_hash, transition_hash, action, subject] =>
            {
                ensure_governance_parse_capacity(
                    state.audits.len(),
                    MAX_GOVERNANCE_AUDITS,
                    "audit",
                    index + 1,
                )?;
                let sequence_text = *sequence;
                let sequence = sequence_text
                    .parse::<u64>()
                    .map_err(|_| admin_error("invalid_registry_audit", "invalid audit sequence"))?;
                if sequence.to_string() != sequence_text {
                    return Err(admin_error(
                        "invalid_registry_audit",
                        "audit sequence must use canonical unsigned decimal encoding",
                    ));
                }
                if !is_canonical_sha256_hex(event_hash) {
                    return Err(admin_error(
                        "invalid_registry_audit",
                        "invalid audit event hash",
                    ));
                }
                if !is_canonical_sha256_hex(before_transition_hash)
                    || !is_canonical_sha256_hex(transition_hash)
                {
                    return Err(admin_error(
                        "invalid_registry_audit",
                        "invalid audit transition hash",
                    ));
                }
                if previous_hash != &GOVERNANCE_AUDIT_ROOT
                    && !is_canonical_sha256_hex(previous_hash)
                {
                    return Err(admin_error(
                        "invalid_registry_audit",
                        "invalid previous audit hash",
                    ));
                }
                validate_governance_name(action, "audit action")?;
                if subject.len() > 256
                    || !subject.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_' | b'.')
                    })
                {
                    return Err(admin_error(
                        "invalid_registry_audit",
                        "invalid audit subject",
                    ));
                }
                state.audits.push(GovernanceAudit {
                    sequence,
                    previous_hash: (*previous_hash).to_string(),
                    event_hash: (*event_hash).to_string(),
                    before_transition_hash: (*before_transition_hash).to_string(),
                    transition_hash: (*transition_hash).to_string(),
                    action: (*action).to_string(),
                    subject: (*subject).to_string(),
                });
            }
            _ => {
                return Err(admin_error(
                    "invalid_registry_governance",
                    format!(
                        "registry governance line {} has an invalid record",
                        index + 1
                    ),
                ))
            }
        }
    }
    if !saw_schema {
        return Err(admin_error(
            "invalid_registry_governance",
            "registry governance schema is missing",
        ));
    }
    validate_governance_state(&state)?;
    Ok(state)
}

fn serialize_governance_state(state: &GovernanceState) -> KuResult<Vec<u8>> {
    validate_governance_state(state)?;
    let mut output = format!("{GOVERNANCE_SCHEMA}\n");
    for developer in &state.developers {
        output.push_str(&format!("developer {developer}\n"));
    }
    for team in &state.teams {
        output.push_str(&format!("team {team}\n"));
    }
    for (team, developer) in &state.members {
        output.push_str(&format!("member {team} {developer}\n"));
    }
    for (package, owner) in &state.owners {
        output.push_str(&format!("owner {package} {}\n", owner_text(owner)));
    }
    for token in &state.tokens {
        let scope = match &token.scope {
            GovernanceTokenScope::All => "all".to_string(),
            GovernanceTokenScope::Package(package_name) => format!("package:{package_name}"),
        };
        output.push_str(&format!(
            "token sha256-{} {} {scope}\n",
            encode_hex(&token.token_hash),
            token.developer
        ));
    }
    for event in &state.audits {
        output.push_str(&format!(
            "audit {} {} {} {} {} {} {}\n",
            event.sequence,
            event.previous_hash,
            event.event_hash,
            event.before_transition_hash,
            event.transition_hash,
            event.action,
            event.subject
        ));
    }
    if output.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "registry_credentials_full",
            "registry governance file reached its size limit",
        ));
    }
    Ok(output.into_bytes())
}

struct GovernanceCredentialIndexes<'a> {
    all_hashes_by_developer: HashMap<&'a str, Vec<[u8; 32]>>,
    package_tokens: Vec<(&'a GovernanceToken, &'a str)>,
    members_by_team: HashMap<&'a str, HashSet<&'a str>>,
    all_token_members_by_team: HashMap<&'a str, Vec<&'a str>>,
}

fn governance_credential_indexes(state: &GovernanceState) -> GovernanceCredentialIndexes<'_> {
    let mut all_hashes_by_developer = HashMap::<&str, Vec<[u8; 32]>>::new();
    let mut package_tokens = Vec::new();
    for token in &state.tokens {
        match &token.scope {
            GovernanceTokenScope::All => all_hashes_by_developer
                .entry(token.developer.as_str())
                .or_default()
                .push(token.token_hash),
            GovernanceTokenScope::Package(package) => {
                package_tokens.push((token, package.as_str()));
            }
        }
    }

    let mut members_by_team = HashMap::<&str, HashSet<&str>>::new();
    for (team, developer) in &state.members {
        members_by_team
            .entry(team.as_str())
            .or_default()
            .insert(developer.as_str());
    }
    let mut all_token_members_by_team = HashMap::<&str, Vec<&str>>::new();
    for (team, developer) in &state.members {
        if all_hashes_by_developer.contains_key(developer.as_str()) {
            all_token_members_by_team
                .entry(team.as_str())
                .or_default()
                .push(developer.as_str());
        }
    }

    GovernanceCredentialIndexes {
        all_hashes_by_developer,
        package_tokens,
        members_by_team,
        all_token_members_by_team,
    }
}

fn governance_credentials(state: &GovernanceState) -> KuResult<Vec<CredentialRecord>> {
    let GovernanceCredentialIndexes {
        all_hashes_by_developer,
        package_tokens,
        members_by_team,
        all_token_members_by_team,
    } = governance_credential_indexes(state);

    let mut records = Vec::new();
    let mut seen_records = HashSet::new();
    for (package, owner) in &state.owners {
        match owner {
            GovernanceOwner::Developer(developer) => {
                if let Some(hashes) = all_hashes_by_developer.get(developer.as_str()) {
                    for token_hash in hashes {
                        push_governance_record(
                            &mut records,
                            &mut seen_records,
                            *token_hash,
                            package,
                        )?;
                    }
                }
            }
            GovernanceOwner::Team(team) => {
                if let Some(members) = all_token_members_by_team.get(team.as_str()) {
                    for developer in members {
                        if let Some(hashes) = all_hashes_by_developer.get(developer) {
                            for token_hash in hashes {
                                push_governance_record(
                                    &mut records,
                                    &mut seen_records,
                                    *token_hash,
                                    package,
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }
    for (token, package) in package_tokens {
        let Some(owner) = state.owners.get(package) else {
            continue;
        };
        let allowed = match owner {
            GovernanceOwner::Developer(developer) => developer == &token.developer,
            GovernanceOwner::Team(team) => members_by_team
                .get(team.as_str())
                .is_some_and(|members| members.contains(token.developer.as_str())),
        };
        if allowed {
            push_governance_record(&mut records, &mut seen_records, token.token_hash, package)?;
        }
    }
    Ok(records)
}

fn push_governance_record<'a>(
    records: &mut Vec<CredentialRecord>,
    seen_records: &mut HashSet<([u8; 32], &'a str)>,
    token_hash: [u8; 32],
    package_name: &'a str,
) -> KuResult<()> {
    if !seen_records.insert((token_hash, package_name)) {
        return Ok(());
    }
    if records.len() >= MAX_REGISTRY_INDEX_VERSIONS {
        return Err(admin_error(
            "invalid_registry_governance",
            "registry governance expands beyond the bounded effective ACL limit",
        ));
    }
    records.push(CredentialRecord {
        token_hash,
        package_name: package_name.to_string(),
    });
    Ok(())
}

fn first_registry_record(bytes: &[u8]) -> KuResult<Option<&str>> {
    let source = std::str::from_utf8(bytes).map_err(|_| {
        admin_error(
            "invalid_registry_credentials",
            "registry credentials file must be valid UTF-8",
        )
    })?;
    Ok(source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#')))
}

fn require_governance_state(bytes: &[u8]) -> KuResult<GovernanceState> {
    if first_registry_record(bytes)? != Some(GOVERNANCE_SCHEMA) {
        return Err(admin_error(
            "registry_governance_migration_required",
            "registry credentials must be migrated to governance schema 2 first",
        ));
    }
    parse_governance_state(bytes)
}

fn owner_has_active_token(
    state: &GovernanceState,
    package_name: &str,
    owner: &GovernanceOwner,
) -> bool {
    let scope_matches = |token: &GovernanceToken| match &token.scope {
        GovernanceTokenScope::All => true,
        GovernanceTokenScope::Package(scoped_package) => scoped_package == package_name,
    };
    match owner {
        GovernanceOwner::Developer(owner) => state
            .tokens
            .iter()
            .any(|token| owner == &token.developer && scope_matches(token)),
        GovernanceOwner::Team(team) => {
            let members = state
                .members
                .iter()
                .filter_map(|(member_team, developer)| {
                    (member_team == team).then_some(developer.as_str())
                })
                .collect::<HashSet<_>>();
            state
                .tokens
                .iter()
                .any(|token| members.contains(token.developer.as_str()) && scope_matches(token))
        }
    }
}

fn all_packages_remain_authorized(state: &GovernanceState) -> bool {
    governance_credentials(state)
        .map(|credentials| {
            credentials
                .iter()
                .map(|credential| credential.package_name.as_str())
                .collect::<BTreeSet<_>>()
                .len()
                == state.owners.len()
        })
        .unwrap_or(false)
}

fn validate_audit_subject(subject: &str) -> KuResult<()> {
    if subject.is_empty()
        || subject.len() > 256
        || !subject
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_' | b'.'))
    {
        return Err(admin_error(
            "invalid_registry_audit",
            "invalid registry governance audit subject",
        ));
    }
    Ok(())
}

fn mutate_governance_from_env(
    action: &str,
    subject: &str,
    mutation: impl FnOnce(&mut GovernanceState) -> KuResult<()>,
) -> KuResult<()> {
    validate_governance_name(action, "audit action")?;
    validate_audit_subject(subject)?;
    let path = normalize_credentials_path(&required_credentials_path()?)?;
    let deadline = absolute_deadline(ADMIN_LOCK_TIMEOUT)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    let mut state = require_governance_state(&original)?;
    mutation(&mut state)?;
    if !all_packages_remain_authorized(&state) {
        return Err(admin_error(
            "registry_owner_has_no_active_token",
            "every package owner must retain at least one active publishing token",
        ));
    }
    append_governance_audit(&mut state, action, subject);
    let replacement = serialize_governance_state(&state)?;
    write_credentials_atomically(&path, &replacement)
}

fn change_package_owner_from_env(package_name: &str, owner: &str, transfer: bool) -> KuResult<()> {
    validate_admin_package_name(package_name)?;
    let owner = parse_owner(owner)?;
    let subject = format!("{package_name}:{}", owner_text(&owner));
    mutate_governance_from_env(
        if transfer {
            "package-transfer"
        } else {
            "package-claim"
        },
        &subject,
        move |state| {
            let owner_exists = match &owner {
                GovernanceOwner::Developer(name) => state.developers.contains(name),
                GovernanceOwner::Team(name) => state.teams.contains(name),
            };
            if !owner_exists {
                return Err(admin_error(
                    "registry_owner_not_found",
                    "package owner does not exist",
                ));
            }
            if !owner_has_active_token(state, package_name, &owner) {
                return Err(admin_error(
                    "registry_owner_has_no_active_token",
                    "package owner must have at least one active publishing token",
                ));
            }
            match state.owners.get(package_name) {
                None if transfer => Err(admin_error(
                    "registry_package_not_owned",
                    "package must be claimed before it can be transferred",
                )),
                Some(_) if !transfer => Err(admin_error(
                    "registry_package_owned",
                    "package is already owned; use package transfer",
                )),
                Some(existing) if existing == &owner => Err(admin_error(
                    "registry_package_owner_unchanged",
                    "package already has the requested owner",
                )),
                _ => {
                    state.owners.insert(package_name.to_string(), owner);
                    Ok(())
                }
            }
        },
    )
}

pub(crate) fn parse_credential_records(bytes: &[u8]) -> KuResult<Vec<CredentialRecord>> {
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "invalid_registry_file",
            format!(
                "registry credentials must be a regular file no larger than {MAX_CREDENTIAL_FILE_BYTES} bytes"
            ),
        ));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| {
        admin_error(
            "invalid_registry_credentials",
            "registry credentials file must be valid UTF-8",
        )
    })?;
    if source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        == Some(GOVERNANCE_SCHEMA)
    {
        return parse_governance_state(bytes).and_then(|state| governance_credentials(&state));
    }
    let mut credentials = Vec::new();
    let mut seen = HashSet::new();
    for (index, raw_line) in source.lines().enumerate() {
        let Some(record) = parse_credential_line(raw_line, index + 1)? else {
            continue;
        };
        if credentials.len() >= MAX_REGISTRY_INDEX_VERSIONS {
            return Err(admin_error(
                "invalid_registry_credentials",
                "registry credentials file has too many ACL entries",
            ));
        }
        if !seen.insert((record.token_hash, record.package_name.clone())) {
            return Err(admin_error(
                "invalid_registry_credentials",
                format!("registry credentials line {} is duplicated", index + 1),
            ));
        }
        credentials.push(record);
    }
    Ok(credentials)
}

fn parse_credential_line(raw_line: &str, line_number: usize) -> KuResult<Option<CredentialRecord>> {
    if raw_line.len() > MAX_REGISTRY_LINE_BYTES {
        return Err(admin_error(
            "invalid_registry_credentials",
            format!("registry credentials line {line_number} is too long"),
        ));
    }
    let line = raw_line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut fields = line.split_ascii_whitespace();
    let (Some(hash), Some(package_name), None) = (fields.next(), fields.next(), fields.next())
    else {
        return Err(admin_error(
            "invalid_registry_credentials",
            format!(
                "registry credentials line {line_number} must be '<sha256-token-hash> <exact-package-name>'"
            ),
        ));
    };
    let Some(hash) = hash.strip_prefix("sha256-") else {
        return Err(invalid_hash_error(line_number));
    };
    let token_hash = decode_hex_array::<32>(hash).map_err(|_| invalid_hash_error(line_number))?;
    validate_package_name(package_name, Span::default()).map_err(|_| {
        admin_error(
            "invalid_registry_credentials",
            format!("registry credentials line {line_number} has an invalid package name"),
        )
    })?;
    Ok(Some(CredentialRecord {
        token_hash,
        package_name: package_name.to_string(),
    }))
}

fn invalid_hash_error(line_number: usize) -> KuError {
    admin_error(
        "invalid_registry_credentials",
        format!("registry credentials line {line_number} has an invalid token hash"),
    )
}

fn issue_token(path: &Path, package_name: &str, timeout: Duration) -> KuResult<String> {
    issue_token_with_writer(path, package_name, timeout, write_credentials_atomically)
}

fn issue_token_with_writer(
    path: &Path,
    package_name: &str,
    timeout: Duration,
    writer: impl FnOnce(&Path, &[u8]) -> KuResult<()>,
) -> KuResult<String> {
    validate_admin_package_name(package_name)?;
    let path = normalize_credentials_path(path)?;
    let deadline = absolute_deadline(timeout)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    if first_registry_record(&original)? == Some(GOVERNANCE_SCHEMA) {
        return Err(admin_error(
            "registry_governance_command_required",
            "use developer token-issue after governance migration",
        ));
    }
    let records = parse_credential_records(&original)?;
    if records.len() >= MAX_REGISTRY_INDEX_VERSIONS {
        return Err(admin_error(
            "registry_credentials_full",
            "registry credentials file has reached its ACL entry limit",
        ));
    }

    for _ in 0..TOKEN_GENERATION_ATTEMPTS {
        let token = generate_token()?;
        let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if records.iter().any(|record| record.token_hash == token_hash) {
            continue;
        }
        let line = format!("sha256-{} {package_name}\n", encode_hex(&token_hash));
        let mut replacement = original.clone();
        if !replacement.is_empty() && !replacement.ends_with(b"\n") {
            replacement.push(b'\n');
        }
        replacement.extend_from_slice(line.as_bytes());
        validate_replacement(&replacement)?;
        writer(&path, &replacement).map_err(|err| {
            annotate_committed_credential_error(err, "issued", "package", package_name, &token_hash)
        })?;
        return Ok(token);
    }
    Err(admin_error(
        "registry_random_failed",
        "failed to generate a unique registry token",
    ))
}

fn revoke_token(path: &Path, package_name: &str, token: &[u8], timeout: Duration) -> KuResult<()> {
    revoke_token_with_writer(
        path,
        package_name,
        token,
        timeout,
        write_credentials_atomically,
    )
}

fn revoke_token_with_writer(
    path: &Path,
    package_name: &str,
    token: &[u8],
    timeout: Duration,
    writer: impl FnOnce(&Path, &[u8]) -> KuResult<()>,
) -> KuResult<()> {
    validate_admin_package_name(package_name)?;
    let path = normalize_credentials_path(path)?;
    let deadline = absolute_deadline(timeout)?;
    let _lock = CredentialFileLock::acquire(&path, deadline)?;
    let original = read_credentials_if_present(&path)?;
    if first_registry_record(&original)? == Some(GOVERNANCE_SCHEMA) {
        return Err(admin_error(
            "registry_governance_command_required",
            "use developer token-revoke after governance migration",
        ));
    }
    parse_credential_records(&original)?;
    let token_hash: [u8; 32] = Sha256::digest(token).into();
    let source = std::str::from_utf8(&original).map_err(|_| {
        admin_error(
            "invalid_registry_credentials",
            "registry credentials file must be valid UTF-8",
        )
    })?;
    let mut replacement = Vec::with_capacity(original.len());
    let mut removed = false;
    for (index, segment) in source.split_inclusive('\n').enumerate() {
        let raw_line = segment.strip_suffix('\n').unwrap_or(segment);
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let record = parse_credential_line(raw_line, index + 1)?;
        if record.as_ref().is_some_and(|record| {
            record.token_hash == token_hash && record.package_name == package_name
        }) {
            removed = true;
        } else {
            replacement.extend_from_slice(segment.as_bytes());
        }
    }
    if !removed {
        return Err(admin_error(
            "registry_credential_not_found",
            "the supplied registry token has no credential for the requested package; it may already have been revoked by a previous committed attempt",
        ));
    }
    if parse_credential_records(&replacement)?.is_empty() {
        return Err(admin_error(
            "registry_last_credential",
            "cannot revoke the last registry credential; issue a replacement credential first",
        ));
    }
    validate_replacement(&replacement)?;
    writer(&path, &replacement).map_err(|err| {
        annotate_committed_credential_error(err, "revoked", "package", package_name, &token_hash)
    })
}

fn annotate_committed_credential_error(
    mut error: KuError,
    action: &str,
    subject_kind: &str,
    subject: &str,
    token_hash: &[u8; 32],
) -> KuError {
    if error.code.as_deref() == Some("registry_credentials_commit_uncertain") {
        error.message.push_str(&format!(
            "; {action} credential sha256-{} for {subject_kind} {subject}; inspect this exact hash before retrying, since the replacement is already visible and its durable state is uncertain",
            encode_hex(token_hash)
        ));
    }
    error
}

fn validate_replacement(bytes: &[u8]) -> KuResult<()> {
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "registry_credentials_full",
            format!("registry credentials file would exceed {MAX_CREDENTIAL_FILE_BYTES} bytes"),
        ));
    }
    parse_credential_records(bytes).map(|_| ())
}

fn generate_token() -> KuResult<String> {
    let mut random = [0u8; TOKEN_RANDOM_BYTES];
    getrandom::fill(&mut random).map_err(|_| {
        admin_error(
            "registry_random_failed",
            "failed to generate a registry token from the operating system random source",
        )
    })?;
    Ok(format!("ku_{}", encode_hex(&random)))
}

fn validate_presented_token(token: &str) -> KuResult<()> {
    if token.is_empty()
        || token.len() > MAX_PUBLISH_TOKEN_BYTES
        || !token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
    {
        return Err(admin_error(
            "invalid_registry_token",
            format!(
                "{REGISTRY_TOKEN_ENV} must be a non-empty ASCII bearer token of at most {MAX_PUBLISH_TOKEN_BYTES} bytes"
            ),
        ));
    }
    Ok(())
}

fn parse_admin_token_hash(value: &str) -> KuResult<[u8; 32]> {
    let encoded = value.strip_prefix("sha256-").ok_or_else(|| {
        admin_error(
            "invalid_registry_token_hash",
            "registry token recovery requires exactly sha256-<64 hex digits>",
        )
    })?;
    decode_hex_array::<32>(encoded).map_err(|_| {
        admin_error(
            "invalid_registry_token_hash",
            "registry token recovery requires exactly sha256-<64 hex digits>",
        )
    })
}

fn is_canonical_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_admin_package_name(package_name: &str) -> KuResult<()> {
    validate_package_name(package_name, Span::default()).map_err(|_| {
        admin_error(
            "invalid_registry_package_name",
            "registry token administration requires one exact valid package name",
        )
    })
}

fn required_credentials_path() -> KuResult<PathBuf> {
    let path = env::var_os(REGISTRY_CREDENTIALS_FILE_ENV).ok_or_else(|| {
        admin_error(
            "missing_registry_config",
            format!(
                "required registry environment variable {REGISTRY_CREDENTIALS_FILE_ENV} is not set"
            ),
        )
    })?;
    if path.is_empty() {
        return Err(admin_error(
            "invalid_registry_file",
            format!("{REGISTRY_CREDENTIALS_FILE_ENV} must not be empty"),
        ));
    }
    Ok(PathBuf::from(path))
}

fn normalize_credentials_path(path: &Path) -> KuResult<PathBuf> {
    let file_name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            admin_error(
                "invalid_registry_file",
                "registry credentials path must name a file",
            )
        })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let parent = match parent {
        Some(parent) => parent.to_path_buf(),
        None => env::current_dir().map_err(|err| {
            admin_error(
                "invalid_registry_file",
                format!("failed to resolve registry credentials directory: {err}"),
            )
        })?,
    };
    let metadata = fs::symlink_metadata(&parent).map_err(|err| {
        admin_error(
            "invalid_registry_file",
            format!("failed to inspect registry credentials directory: {err}"),
        )
    })?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return Err(admin_error(
            "invalid_registry_file",
            "registry credentials directory must be a real directory",
        ));
    }
    let parent = fs::canonicalize(&parent).map_err(|err| {
        admin_error(
            "invalid_registry_file",
            format!("failed to resolve registry credentials directory: {err}"),
        )
    })?;
    Ok(parent.join(file_name))
}

fn read_credentials_if_present(path: &Path) -> KuResult<Vec<u8>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(admin_error(
                "invalid_registry_file",
                format!("failed to inspect registry credentials file: {err}"),
            ));
        }
    };
    validate_regular_metadata(&metadata, "credentials")?;
    if metadata.len() > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "invalid_registry_file",
            format!(
                "registry credentials must be a regular file no larger than {MAX_CREDENTIAL_FILE_BYTES} bytes"
            ),
        ));
    }
    let file = open_nofollow(path, false, false).map_err(|err| {
        admin_error(
            "invalid_registry_file",
            format!("failed to open registry credentials file: {err}"),
        )
    })?;
    let opened = file.metadata().map_err(|err| {
        admin_error(
            "invalid_registry_file",
            format!("failed to inspect opened registry credentials file: {err}"),
        )
    })?;
    validate_regular_metadata(&opened, "credentials")?;
    if opened.len() != metadata.len() || opened.len() > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "invalid_registry_file",
            "registry credentials file changed while it was opened",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CREDENTIAL_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| {
            admin_error(
                "invalid_registry_file",
                format!("failed to read registry credentials file: {err}"),
            )
        })?;
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(admin_error(
            "invalid_registry_file",
            format!("registry credentials file exceeds {MAX_CREDENTIAL_FILE_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

#[derive(Debug)]
struct CredentialFileLock {
    file: fs::File,
}

impl CredentialFileLock {
    fn acquire(credentials_path: &Path, deadline: Instant) -> KuResult<Self> {
        let lock_path = sibling_lock_path(credentials_path)?;
        if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
            validate_regular_metadata(&metadata, "credentials lock")?;
        }
        let file = open_nofollow(&lock_path, true, true).map_err(|err| {
            admin_error(
                "registry_credentials_lock_failed",
                format!("failed to open registry credentials lock: {err}"),
            )
        })?;
        let opened = file.metadata().map_err(|err| {
            admin_error(
                "registry_credentials_lock_failed",
                format!("failed to inspect registry credentials lock: {err}"),
            )
        })?;
        validate_regular_metadata(&opened, "credentials lock")?;
        ensure_lock_path_identity(&lock_path, &file)?;
        loop {
            if Instant::now() >= deadline {
                return Err(credentials_lock_timeout_error());
            }
            match file.try_lock() {
                Ok(()) => {
                    ensure_lock_path_identity(&lock_path, &file)?;
                    if Instant::now() >= deadline {
                        let _ = file.unlock();
                        return Err(credentials_lock_timeout_error());
                    }
                    return Ok(Self { file });
                }
                Err(fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(credentials_lock_timeout_error());
                    }
                    thread::sleep(remaining.min(ADMIN_LOCK_POLL));
                }
                Err(fs::TryLockError::Error(err)) => {
                    return Err(admin_error(
                        "registry_credentials_lock_failed",
                        format!("failed to acquire registry credentials lock: {err}"),
                    ));
                }
            }
        }
    }
}

fn credentials_lock_timeout_error() -> KuError {
    admin_error(
        "registry_credentials_lock_timeout",
        "registry credentials lock wait exceeded its absolute deadline",
    )
}

fn ensure_lock_path_identity(path: &Path, opened: &fs::File) -> KuResult<()> {
    let current = open_nofollow(path, false, true).map_err(|err| {
        admin_error(
            "registry_credentials_lock_failed",
            format!("failed to re-open registry credentials lock: {err}"),
        )
    })?;
    let metadata = current.metadata().map_err(|err| {
        admin_error(
            "registry_credentials_lock_failed",
            format!("failed to inspect current registry credentials lock: {err}"),
        )
    })?;
    validate_regular_metadata(&metadata, "credentials lock")?;
    if file_identity(opened)? != file_identity(&current)? {
        return Err(admin_error(
            "registry_credentials_lock_replaced",
            "registry credentials lock was replaced while it was opened",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    file: u64,
}

#[cfg(unix)]
fn file_identity(file: &fs::File) -> KuResult<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|err| {
        admin_error(
            "registry_credentials_lock_failed",
            format!("failed to identify registry credentials lock: {err}"),
        )
    })?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(file: &fs::File) -> KuResult<FileIdentity> {
    use std::{mem::MaybeUninit, os::windows::io::AsRawHandle};

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            information: *mut FileInformation,
        ) -> i32;
    }

    let mut information = MaybeUninit::<FileInformation>::uninit();
    // SAFETY: `file` owns a valid handle for the call, and Windows initializes
    // the complete structure before returning a nonzero status.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(admin_error(
            "registry_credentials_lock_failed",
            format!(
                "failed to identify registry credentials lock: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    // SAFETY: the preceding OS call returned success.
    let information = unsafe { information.assume_init() };
    Ok(FileIdentity {
        volume: u64::from(information.volume_serial_number),
        file: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(file: &fs::File) -> KuResult<FileIdentity> {
    let metadata = file.metadata().map_err(|err| {
        admin_error(
            "registry_credentials_lock_failed",
            format!("failed to identify registry credentials lock: {err}"),
        )
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos() as u64)
        .unwrap_or_default();
    Ok(FileIdentity {
        volume: metadata.len(),
        file: modified,
    })
}

impl Drop for CredentialFileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn sibling_lock_path(credentials_path: &Path) -> KuResult<PathBuf> {
    let file_name = credentials_path.file_name().ok_or_else(|| {
        admin_error(
            "invalid_registry_file",
            "registry credentials path must name a file",
        )
    })?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(credentials_path.with_file_name(lock_name))
}

struct CredentialFilePermissions {
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    mode: u32,
    #[cfg(windows)]
    dacl: credential_dacl::Dacl,
    #[cfg(not(any(unix, windows)))]
    permissions: fs::Permissions,
}

impl CredentialFilePermissions {
    fn read_existing(path: &Path) -> KuResult<Option<Self>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(credential_permissions_error("inspect original", err)),
        };
        validate_regular_metadata(&metadata, "credentials")?;
        let file = open_nofollow(path, false, false)
            .map_err(|err| credential_permissions_error("open original", err))?;
        let metadata = file
            .metadata()
            .map_err(|err| credential_permissions_error("inspect opened original", err))?;
        validate_regular_metadata(&metadata, "credentials")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure_credential_file_has_no_extended_acl(&file, "original")?;
            Ok(Some(Self {
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: metadata.mode() & 0o7777,
            }))
        }
        #[cfg(windows)]
        {
            Ok(Some(Self {
                dacl: credential_dacl::Dacl::read(&file)
                    .map_err(|err| credential_permissions_error("read original DACL", err))?,
            }))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Some(Self {
                permissions: metadata.permissions(),
            }))
        }
    }

    fn apply(&self, file: &fs::File) -> KuResult<()> {
        #[cfg(unix)]
        {
            use std::os::unix::{
                fs::{MetadataExt, PermissionsExt},
                io::AsRawFd,
            };
            let metadata = file
                .metadata()
                .map_err(|err| credential_permissions_error("inspect staging ownership", err))?;
            if metadata.uid() != self.uid || metadata.gid() != self.gid {
                // SAFETY: the live File owns the descriptor; these are the
                // validated original file's uid/gid, not path-selected targets.
                if unsafe { libc::fchown(file.as_raw_fd(), self.uid, self.gid) } != 0 {
                    return Err(credential_permissions_error(
                        "preserve original uid/gid on staging",
                        io::Error::last_os_error(),
                    ));
                }
            }
            file.set_permissions(fs::Permissions::from_mode(self.mode))
                .map_err(|err| {
                    credential_permissions_error("preserve original mode on staging", err)
                })?;
            let applied = file
                .metadata()
                .map_err(|err| credential_permissions_error("verify staging permissions", err))?;
            if applied.uid() != self.uid
                || applied.gid() != self.gid
                || applied.mode() & 0o7777 != self.mode
            {
                return Err(credential_permissions_error(
                    "verify staging permissions",
                    io::Error::other("uid/gid/mode did not match"),
                ));
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            self.dacl.apply(file).map_err(|err| {
                credential_permissions_error("preserve original DACL on staging", err)
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            file.set_permissions(self.permissions.clone())
                .map_err(|err| credential_permissions_error("preserve original permissions", err))
        }
    }
}

fn credential_permissions_error(action: &str, error: io::Error) -> KuError {
    admin_error(
        "registry_credentials_permissions_failed",
        format!("failed to {action} before registry credentials replacement: {error}"),
    )
}

#[cfg(unix)]
fn credential_acl_unsupported(role: &str, reason: &str) -> KuError {
    admin_error(
        "registry_credentials_acl_unsupported",
        format!(
            "{role} registry credentials file {reason}; replacement stopped before writing credentials; use a trusted credential directory with reliably inspectable ordinary Unix permissions"
        ),
    )
}

#[cfg(target_os = "linux")]
fn ensure_credential_file_has_no_extended_acl(file: &fs::File, role: &str) -> KuResult<()> {
    use std::os::unix::io::AsRawFd;

    // A mode-only POSIX ACL has one 4-byte header and exactly three 8-byte
    // entries. One fixed-size read rejects larger ACLs without allocating,
    // retrying or racing a separate size query against the attribute read.
    let mut value = [0u8; 28];
    // SAFETY: the File owns a live fd, the attribute name is NUL terminated,
    // and value provides the full writable capacity passed to fgetxattr.
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"system.posix_acl_access".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if size < 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENODATA) => return Ok(()),
            // Unsupported inspection does not prove absence of access rules.
            Some(libc::ENOTSUP) => {
                return Err(credential_acl_unsupported(
                    role,
                    "does not support reliable access ACL inspection",
                ));
            }
            // An ACL too large for three base entries is necessarily extended.
            Some(libc::ERANGE) => {
                return Err(credential_acl_unsupported(
                    role,
                    "has an extended access ACL that cannot be preserved safely",
                ));
            }
            _ => {
                return Err(credential_permissions_error(
                    &format!("inspect {role} access ACL"),
                    error,
                ));
            }
        }
    }
    if size as usize == value.len() && linux_access_acl_is_mode_only(&value) {
        Ok(())
    } else {
        // Unknown/malformed encodings must not silently become mode bits.
        Err(credential_acl_unsupported(
            role,
            "has an extended or unrecognized access ACL that cannot be preserved safely",
        ))
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_access_acl_is_mode_only(value: &[u8]) -> bool {
    // Linux UAPI: posix_acl_xattr.h version 2, little-endian fields;
    // posix_acl.h USER_OBJ=1, GROUP_OBJ=4, OTHER=32, undefined id=-1.
    if value.len() != 28 || value[..4] != 2u32.to_le_bytes() {
        return false;
    }
    value[4..]
        .chunks_exact(8)
        .zip([1u16, 4, 32])
        .all(|(entry, expected_tag)| {
            u16::from_le_bytes([entry[0], entry[1]]) == expected_tag
                && u16::from_le_bytes([entry[2], entry[3]]) <= 7
                && entry[4..] == u32::MAX.to_le_bytes()
        })
}

#[cfg(target_os = "macos")]
fn ensure_credential_file_has_no_extended_acl(file: &fs::File, role: &str) -> KuResult<()> {
    credential_macos_acl::ensure_mode_only(file, role)
}

#[cfg(any(target_os = "macos", test))]
fn macos_extended_acl_header_is_mode_only(header: &[u32; 11], size: isize) -> bool {
    // Darwin acl_copy_ext's portable header has magic, two 16-byte UUIDs,
    // entry count and flags. Empty ACLs with flags (including deferred
    // inheritance on rename) must not be treated as ordinary mode bits.
    size == 44 && u32::from_be(header[0]) == 0x012c_c16d && header[9] == 0 && header[10] == 0
}

#[cfg(target_os = "macos")]
mod credential_macos_acl {
    use super::{
        credential_acl_unsupported, credential_permissions_error,
        macos_extended_acl_header_is_mode_only,
    };
    use crate::error::KuResult;
    use std::{ffi::c_void, fs, io, os::unix::io::AsRawFd};

    // Darwin libc exports these APIs in libSystem, but libc's Rust bindings
    // do not expose them. Values and signatures are from Apple's sys/acl.h.
    const ACL_TYPE_EXTENDED: libc::c_int = 0x100;

    #[link(name = "System")]
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut c_void;
        fn acl_copy_ext(
            buffer: *mut c_void,
            acl: *mut c_void,
            size: libc::ssize_t,
        ) -> libc::ssize_t;
        fn acl_free(acl: *mut c_void) -> libc::c_int;
        #[cfg(test)]
        fn acl_from_text(text: *const libc::c_char) -> *mut c_void;
        #[cfg(test)]
        fn acl_set_fd(fd: libc::c_int, acl: *mut c_void) -> libc::c_int;
    }

    struct Acl(*mut c_void);

    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: this is the unique owner of an ACL returned by libc.
            unsafe { acl_free(self.0) };
        }
    }

    pub(super) fn ensure_mode_only(file: &fs::File, role: &str) -> KuResult<()> {
        // SAFETY: the descriptor is owned by the live File. Use EXTENDED, not
        // the unsupported POSIX ACCESS type mentioned by older Darwin manpages.
        let raw = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            // Darwin filesec_get_property(FILESEC_ACL) reports ENOENT when
            // fstatx_np found no ACL on this already-open descriptor.
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            if error.raw_os_error() == Some(libc::ENOTSUP)
                || error.raw_os_error() == Some(libc::EOPNOTSUPP)
            {
                return Err(credential_acl_unsupported(
                    role,
                    "does not support reliable extended ACL inspection",
                ));
            }
            return Err(credential_permissions_error(
                &format!("inspect {role} extended ACL"),
                error,
            ));
        }
        let acl = Acl(raw);
        // acl_copy_ext validates the ACL and rejects anything larger than the
        // empty 44-byte header. u32 storage supplies the C header's alignment;
        // no opaque libc structs are dereferenced and no ACE loop is needed.
        let mut header = [0u32; 11];
        // SAFETY: acl is alive, and header is a 44-byte, aligned writable buffer.
        let size = unsafe { acl_copy_ext(header.as_mut_ptr().cast(), acl.0, 44) };
        if size < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ERANGE) {
                return Err(credential_permissions_error(
                    &format!("inspect {role} extended ACL header"),
                    error,
                ));
            }
        }
        if macos_extended_acl_header_is_mode_only(&header, size) {
            Ok(())
        } else {
            Err(credential_acl_unsupported(
                role,
                "has extended ACL entries or flags that cannot be preserved safely",
            ))
        }
    }

    #[cfg(test)]
    pub(super) fn install_test_acl(file: &fs::File, text: &std::ffi::CStr) -> io::Result<()> {
        // The fixtures use a fixed UUID, so ACL creation does not require
        // shell commands, directory-service lookups or account privileges.
        let raw = unsafe { acl_from_text(text.as_ptr()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let acl = Acl(raw);
        if unsafe { acl_set_fd(file.as_raw_fd(), acl.0) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn ensure_credential_file_has_no_extended_acl(_file: &fs::File, _role: &str) -> KuResult<()> {
    Err(admin_error(
        "registry_credentials_acl_unsupported",
        "this Unix platform cannot safely inspect credential ACLs; replacement stopped before writing credentials",
    ))
}

#[cfg(windows)]
mod credential_dacl {
    use std::{ffi::c_void, fs, io, mem, os::windows::io::AsRawHandle, ptr};

    const SE_FILE_OBJECT: u32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 1;
    const GROUP_SECURITY_INFORMATION: u32 = 2;
    const DACL_SECURITY_INFORMATION: u32 = 4;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
    const UNPROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x2000_0000;
    const SE_DACL_PROTECTED: u16 = 0x1000;

    #[repr(C)]
    struct AclSizeInformation {
        ace_count: u32,
        bytes_in_use: u32,
        bytes_free: u32,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            handle: *mut c_void,
            object_type: u32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn SetSecurityInfo(
            handle: *mut c_void,
            object_type: u32,
            information: u32,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *mut c_void,
            sacl: *mut c_void,
        ) -> u32;
        fn GetSecurityDescriptorControl(
            descriptor: *const c_void,
            control: *mut u16,
            revision: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorDacl(
            descriptor: *const c_void,
            present: *mut i32,
            dacl: *mut *mut c_void,
            defaulted: *mut i32,
        ) -> i32;
        fn GetSecurityDescriptorOwner(
            descriptor: *const c_void,
            owner: *mut *mut c_void,
            defaulted: *mut i32,
        ) -> i32;
        fn GetSecurityDescriptorGroup(
            descriptor: *const c_void,
            group: *mut *mut c_void,
            defaulted: *mut i32,
        ) -> i32;
        fn IsValidSid(sid: *const c_void) -> i32;
        fn GetLengthSid(sid: *const c_void) -> u32;
        fn GetAclInformation(
            acl: *const c_void,
            information: *mut c_void,
            length: u32,
            class: u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    pub(super) struct Dacl {
        descriptor: *mut c_void,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Snapshot {
        protected: bool,
        owner: Option<Vec<u8>>,
        group: Option<Vec<u8>>,
        bytes: Option<Vec<u8>>,
    }

    impl Dacl {
        #[cfg(test)]
        pub(super) fn from_sddl(sddl: &str) -> io::Result<Self> {
            #[link(name = "advapi32")]
            unsafe extern "system" {
                fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text: *const u16,
                    revision: u32,
                    descriptor: *mut *mut c_void,
                    descriptor_size: *mut u32,
                ) -> i32;
            }
            let text = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            let mut descriptor = ptr::null_mut();
            // SAFETY: text is terminated UTF-16 and the API returns a
            // LocalAlloc-owned descriptor, released by the same Drop path.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if descriptor.is_null() {
                return Err(io::Error::other("Windows returned a null test descriptor"));
            }
            Ok(Self { descriptor })
        }

        pub(super) fn read(file: &fs::File) -> io::Result<Self> {
            let mut descriptor = ptr::null_mut();
            // SAFETY: the File owns a live handle. GetSecurityInfo allocates
            // the returned descriptor with LocalAlloc; Drop releases it.
            let error = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION
                        | GROUP_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut descriptor,
                )
            };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            if descriptor.is_null() {
                return Err(io::Error::other(
                    "Windows returned a null security descriptor",
                ));
            }
            let security = Self { descriptor };
            let (_, owner, group, dacl_present, _) = security.parts()?;
            if owner.is_null() || group.is_null() || !dacl_present {
                return Err(io::Error::other(
                    "Windows security descriptor omitted owner, group, or DACL",
                ));
            }
            Ok(security)
        }

        fn parts(&self) -> io::Result<(bool, *mut c_void, *mut c_void, bool, *mut c_void)> {
            let mut control = 0u16;
            let mut revision = 0u32;
            let mut present = 0i32;
            let mut defaulted = 0i32;
            let mut owner = ptr::null_mut();
            let mut group = ptr::null_mut();
            let mut dacl = ptr::null_mut();
            // SAFETY: self owns the valid Windows-allocated descriptor and all
            // output pointers refer to initialized, correctly typed locals.
            if unsafe { GetSecurityDescriptorControl(self.descriptor, &mut control, &mut revision) }
                == 0
                || unsafe {
                    GetSecurityDescriptorOwner(self.descriptor, &mut owner, &mut defaulted)
                } == 0
                || unsafe {
                    GetSecurityDescriptorGroup(self.descriptor, &mut group, &mut defaulted)
                } == 0
                || unsafe {
                    GetSecurityDescriptorDacl(
                        self.descriptor,
                        &mut present,
                        &mut dacl,
                        &mut defaulted,
                    )
                } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok((
                control & SE_DACL_PROTECTED != 0,
                owner,
                group,
                present != 0,
                if present == 0 { ptr::null_mut() } else { dacl },
            ))
        }

        fn sid_bytes(sid: *const c_void) -> io::Result<Option<Vec<u8>>> {
            if sid.is_null() {
                return Ok(None);
            }
            if unsafe { IsValidSid(sid) } == 0 {
                return Err(io::Error::other("Windows returned an invalid SID"));
            }
            let length = unsafe { GetLengthSid(sid) };
            if !(8..=68).contains(&length) {
                return Err(io::Error::other("Windows returned an invalid SID length"));
            }
            Ok(Some(
                unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length as usize) }.to_vec(),
            ))
        }

        pub(super) fn snapshot(&self) -> io::Result<Snapshot> {
            let (protected, owner, group, dacl_present, dacl) = self.parts()?;
            if !dacl_present {
                return Err(io::Error::other(
                    "Windows security descriptor omitted the DACL",
                ));
            }
            let bytes = if dacl.is_null() {
                None
            } else {
                let mut size = AclSizeInformation {
                    ace_count: 0,
                    bytes_in_use: 0,
                    bytes_free: 0,
                };
                // SAFETY: dacl points into self's live descriptor. Windows
                // fills the fixed-size AclSizeInformation structure.
                if unsafe {
                    GetAclInformation(
                        dacl,
                        (&mut size as *mut AclSizeInformation).cast(),
                        mem::size_of::<AclSizeInformation>() as u32,
                        2,
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if !(8..=65_535).contains(&size.bytes_in_use) {
                    return Err(io::Error::other(
                        "Windows returned an invalid DACL byte length",
                    ));
                }
                // SAFETY: bytes_in_use is the OS-verified initialized ACL
                // range in the owned descriptor, excluding unused padding.
                Some(
                    unsafe {
                        std::slice::from_raw_parts(dacl.cast::<u8>(), size.bytes_in_use as usize)
                    }
                    .to_vec(),
                )
            };
            Ok(Snapshot {
                protected,
                owner: Self::sid_bytes(owner)?,
                group: Self::sid_bytes(group)?,
                bytes,
            })
        }

        pub(super) fn apply(&self, file: &fs::File) -> io::Result<()> {
            let expected = self.snapshot()?;
            let (protected, owner, group, dacl_present, dacl) = self.parts()?;
            if !dacl_present {
                return Err(io::Error::other(
                    "Windows security descriptor omitted the DACL",
                ));
            }
            let mut information = DACL_SECURITY_INFORMATION
                | if protected {
                    PROTECTED_DACL_SECURITY_INFORMATION
                } else {
                    UNPROTECTED_DACL_SECURITY_INFORMATION
                };
            if !owner.is_null() {
                information |= OWNER_SECURITY_INFORMATION;
            }
            if !group.is_null() {
                information |= GROUP_SECURITY_INFORMATION;
            }
            // SAFETY: the staging handle has WRITE_OWNER and WRITE_DAC. The
            // borrowed owner/group/DACL stay live for the call. No
            // ignore-security-error behavior is enabled.
            let error = unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    information,
                    owner,
                    group,
                    dacl,
                    ptr::null_mut(),
                )
            };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            let actual = Self::read(file)?.snapshot()?;
            if actual.protected != expected.protected
                || actual.bytes != expected.bytes
                || expected.owner.is_some() && actual.owner != expected.owner
                || expected.group.is_some() && actual.group != expected.group
            {
                return Err(io::Error::other(
                    "staging DACL did not exactly match the original",
                ));
            }
            Ok(())
        }
    }

    impl Drop for Dacl {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
            unsafe {
                LocalFree(self.descriptor);
            }
        }
    }
}

struct CredentialParentSync {
    #[cfg(not(windows))]
    directory: fs::File,
}

impl CredentialParentSync {
    fn open(parent: &Path) -> KuResult<Self> {
        #[cfg(not(windows))]
        {
            Ok(Self {
                directory: fs::File::open(parent).map_err(|err| {
                    admin_error("registry_credentials_write_failed", format!(
                        "failed to open registry credentials directory for synchronization before replacement: {err}"
                    ))
                })?,
            })
        }
        #[cfg(windows)]
        {
            let _ = parent;
            Ok(Self {})
        }
    }

    fn sync(&self) -> io::Result<()> {
        #[cfg(not(windows))]
        {
            self.directory.sync_all()
        }
        #[cfg(windows)]
        {
            Ok(())
        }
    }
}

fn write_credentials_atomically(path: &Path, bytes: &[u8]) -> KuResult<()> {
    write_credentials_atomically_with_hook(path, bytes, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtomicWriteCheckpoint {
    AfterPermissions,
    BeforeReplace,
    AfterReplace,
}

fn write_credentials_atomically_with_hook(
    path: &Path,
    bytes: &[u8],
    checkpoint: impl FnMut(AtomicWriteCheckpoint) -> io::Result<()>,
) -> KuResult<()> {
    write_credentials_atomically_with_hooks(path, bytes, checkpoint, CredentialParentSync::sync)
}

fn write_credentials_atomically_with_hooks(
    path: &Path,
    bytes: &[u8],
    mut checkpoint: impl FnMut(AtomicWriteCheckpoint) -> io::Result<()>,
    sync_parent: impl FnOnce(&CredentialParentSync) -> io::Result<()>,
) -> KuResult<()> {
    validate_replacement(bytes)?;
    reject_existing_link(path, "credentials")?;
    let parent = path.parent().ok_or_else(|| {
        admin_error(
            "invalid_registry_file",
            "registry credentials path must have a parent directory",
        )
    })?;
    // Predictable directory-open/permission failures must happen before the
    // replacement, while later fsync failures still report a committed state.
    let parent_sync = CredentialParentSync::open(parent)?;
    let permissions = CredentialFilePermissions::read_existing(path)?;
    let temporary_path = staging_path(path)?;
    remove_stale_staging_file(&temporary_path)?;
    let mut temporary = TemporaryCredentialFile::new(temporary_path.clone());
    let mut file = open_new_private_file(&temporary_path).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to create registry credentials staging file: {err}"),
        )
    })?;
    // A parent's default/inheritable ACL can add entries even when creation
    // requested mode 0600. Inspect the actual empty staging fd, including the
    // first issue when no original permissions exist yet.
    #[cfg(unix)]
    ensure_credential_file_has_no_extended_acl(&file, "staging")?;
    if let Some(permissions) = permissions {
        permissions.apply(&file)?;
    }
    checkpoint(AtomicWriteCheckpoint::AfterPermissions).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!(
                "registry credentials write stopped before staging contents were written: {err}"
            ),
        )
    })?;
    file.write_all(bytes).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to write registry credentials staging file: {err}"),
        )
    })?;
    file.sync_all().map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to sync registry credentials staging file: {err}"),
        )
    })?;
    drop(file);
    checkpoint(AtomicWriteCheckpoint::BeforeReplace).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("registry credentials write stopped before atomic replacement: {err}"),
        )
    })?;
    reject_existing_link(path, "credentials")?;
    replace_file_atomically(&temporary_path, path).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to atomically replace registry credentials file: {err}"),
        )
    })?;
    temporary.keep();
    checkpoint(AtomicWriteCheckpoint::AfterReplace)
        .map_err(|err| committed_credentials_error("post-replacement checkpoint", err))?;
    sync_parent(&parent_sync)
        .map_err(|err| committed_credentials_error("directory synchronization", err))
}

fn committed_credentials_error(phase: &str, error: io::Error) -> KuError {
    admin_error(
        "registry_credentials_commit_uncertain",
        format!(
            "registry credentials replacement was committed, but durability is uncertain because {phase} failed ({:?}); do not assume that the change was rolled back",
            error.kind()
        ),
    )
}

fn staging_path(path: &Path) -> KuResult<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        admin_error(
            "invalid_registry_file",
            "registry credentials path must name a file",
        )
    })?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(".tmp");
    Ok(path.with_file_name(temporary_name))
}

fn remove_stale_staging_file(path: &Path) -> KuResult<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(admin_error(
                "registry_credentials_write_failed",
                format!("failed to inspect registry credentials staging file: {err}"),
            ));
        }
    };
    validate_regular_metadata(&metadata, "credentials staging")?;
    fs::remove_file(path).map_err(|err| {
        admin_error(
            "registry_credentials_write_failed",
            format!("failed to remove stale registry credentials staging file: {err}"),
        )
    })
}

struct TemporaryCredentialFile {
    path: PathBuf,
    remove_on_drop: bool,
}

impl TemporaryCredentialFile {
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

impl Drop for TemporaryCredentialFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn reject_existing_link(path: &Path, kind: &str) -> KuResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_regular_metadata(&metadata, kind),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(admin_error(
            "invalid_registry_file",
            format!("failed to inspect registry {kind} file: {err}"),
        )),
    }
}

fn validate_regular_metadata(metadata: &fs::Metadata, kind: &str) -> KuResult<()> {
    if !metadata.is_file() || metadata_is_reparse_point(metadata) {
        return Err(admin_error(
            "invalid_registry_file",
            format!("registry {kind} must be a real regular file"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn open_nofollow(path: &Path, create: bool, write: bool) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_nofollow(path: &Path, create: bool, write: bool) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    fs::OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_nofollow(path: &Path, create: bool, write: bool) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn open_new_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_new_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const WRITE_OWNER: u32 = 0x0008_0000;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_new_private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
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

fn absolute_deadline(timeout: Duration) -> KuResult<Instant> {
    Instant::now().checked_add(timeout).ok_or_else(|| {
        admin_error(
            "registry_credentials_lock_failed",
            "registry credentials lock deadline overflowed",
        )
    })
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

fn decode_hex_array<const N: usize>(value: &str) -> Result<[u8; N], ()> {
    if value.len() != N * 2 {
        return Err(());
    }
    let mut output = [0u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (decode_hex_digit(pair[0])? << 4) | decode_hex_digit(pair[1])?;
    }
    Ok(output)
}

fn decode_hex_digit(value: u8) -> Result<u8, ()> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(()),
    }
}

fn admin_error(code: &str, message: impl Into<String>) -> KuError {
    KuError::package(code, message.into(), Span::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "ku-registry-admin-{label}-{}-{}",
                std::process::id(),
                NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create registry admin test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn command_surface_has_one_issue_and_revoke_form() {
        assert_eq!(
            parse_registry_command(Vec::new()).unwrap(),
            RegistryCommand::Serve
        );
        assert_eq!(
            parse_registry_command([OsString::from("--help")]).unwrap(),
            RegistryCommand::Help
        );
        assert_eq!(
            parse_registry_command(["token", "issue", "math"].map(OsString::from)).unwrap(),
            RegistryCommand::Issue {
                package_name: "math".to_string()
            }
        );
        assert_eq!(
            parse_registry_command(["token", "revoke", "math"].map(OsString::from)).unwrap(),
            RegistryCommand::Revoke {
                package_name: "math".to_string()
            }
        );
        assert_eq!(
            parse_registry_command(
                ["package", "transfer", "math", "team:core"].map(OsString::from)
            )
            .unwrap(),
            RegistryCommand::PackageTransfer {
                package_name: "math".to_string(),
                owner: "team:core".to_string(),
            }
        );
        assert_eq!(
            parse_registry_command(["audit", "verify"].map(OsString::from)).unwrap(),
            RegistryCommand::AuditVerify
        );
        assert_eq!(
            parse_registry_command(
                [
                    "developer",
                    "token-revoke-hash",
                    "alice",
                    &format!("sha256-{}", "ab".repeat(32)),
                ]
                .map(OsString::from)
            )
            .unwrap(),
            RegistryCommand::DeveloperTokenRevokeHash {
                developer: "alice".to_string(),
                token_hash: [0xab; 32],
            }
        );
        for invalid in [
            vec!["issue", "math"],
            vec!["token", "add", "math"],
            vec!["token", "issue", "math", "secret"],
            vec!["developer", "token-revoke-hash", "alice"],
            vec!["developer", "token-revoke-hash", "alice", "ab"],
            vec![
                "developer",
                "token-revoke-hash",
                "alice",
                "sha256-not-a-token-hash",
            ],
        ] {
            assert!(parse_registry_command(invalid.into_iter().map(OsString::from)).is_err());
        }
    }

    fn governance_fixture() -> (GovernanceState, [u8; 32], [u8; 32], [u8; 32]) {
        let alice_hash: [u8; 32] = Sha256::digest(b"alice-token").into();
        let bob_hash: [u8; 32] = Sha256::digest(b"bob-token").into();
        let outsider_hash: [u8; 32] = Sha256::digest(b"outsider-token").into();
        let mut state = GovernanceState::default();
        for (index, developer) in ["alice", "bob", "outsider"].into_iter().enumerate() {
            state.developers.insert(developer.to_string());
            append_governance_audit(
                &mut state,
                if index == 0 {
                    "init"
                } else {
                    "developer-create"
                },
                developer,
            );
        }
        state.teams.insert("core".to_string());
        append_governance_audit(&mut state, "team-create", "core");
        state
            .members
            .insert(("core".to_string(), "bob".to_string()));
        append_governance_audit(&mut state, "team-member-add", "core:bob");
        for (developer, token_hash) in [
            ("alice", alice_hash),
            ("bob", bob_hash),
            ("outsider", outsider_hash),
        ] {
            let token = GovernanceToken {
                token_hash,
                developer: developer.to_string(),
                scope: GovernanceTokenScope::All,
            };
            state.tokens.insert(token.clone());
            append_governance_audit(
                &mut state,
                "developer-token-issue",
                &token_audit_subject(&token),
            );
        }
        state.owners.insert(
            "math".to_string(),
            GovernanceOwner::Developer("alice".to_string()),
        );
        append_governance_audit(&mut state, "package-claim", "math:developer:alice");
        state.owners.insert(
            "tools".to_string(),
            GovernanceOwner::Team("core".to_string()),
        );
        append_governance_audit(&mut state, "package-claim", "tools:team:core");
        (state, alice_hash, bob_hash, outsider_hash)
    }

    #[test]
    fn governance_schema_flattens_exact_owner_and_team_member_authorizations() {
        let (state, alice_hash, bob_hash, outsider_hash) = governance_fixture();
        let encoded = serialize_governance_state(&state).unwrap();
        let parsed = parse_governance_state(&encoded).unwrap();
        assert_eq!(parsed, state);
        let credentials = parse_credential_records(&encoded).unwrap();
        assert_eq!(credentials.len(), 2);
        assert!(credentials.iter().any(|credential| {
            credential.token_hash == alice_hash && credential.package_name == "math"
        }));
        assert!(credentials.iter().any(|credential| {
            credential.token_hash == bob_hash && credential.package_name == "tools"
        }));
        assert!(credentials
            .iter()
            .all(|credential| credential.token_hash != outsider_hash));
    }

    #[test]
    fn migration_scopes_preserve_legacy_acl_without_cartesian_permission_expansion() {
        let math_hash: [u8; 32] = Sha256::digest(b"math-token").into();
        let tools_hash: [u8; 32] = Sha256::digest(b"tools-token").into();
        let legacy = vec![
            CredentialRecord {
                token_hash: math_hash,
                package_name: "math".to_string(),
            },
            CredentialRecord {
                token_hash: tools_hash,
                package_name: "tools".to_string(),
            },
        ];
        let state = migrate_legacy_credentials(legacy, "alice");
        let credentials = parse_credential_records(&serialize_governance_state(&state).unwrap())
            .expect("migration state remains a valid server credential snapshot");
        assert_eq!(credentials.len(), 2);
        assert!(credentials.iter().any(|credential| {
            credential.token_hash == math_hash && credential.package_name == "math"
        }));
        assert!(credentials.iter().any(|credential| {
            credential.token_hash == tools_hash && credential.package_name == "tools"
        }));
        assert!(!credentials.iter().any(|credential| {
            (credential.token_hash == math_hash && credential.package_name == "tools")
                || (credential.token_hash == tools_hash && credential.package_name == "math")
        }));
    }

    #[test]
    fn migration_preserves_one_legacy_token_authorized_for_multiple_packages() {
        let shared_hash: [u8; 32] = Sha256::digest(b"shared-token").into();
        let legacy = vec![
            CredentialRecord {
                token_hash: shared_hash,
                package_name: "math".to_string(),
            },
            CredentialRecord {
                token_hash: shared_hash,
                package_name: "tools".to_string(),
            },
        ];
        let state = migrate_legacy_credentials(legacy.clone(), "alice");
        let encoded = serialize_governance_state(&state).unwrap();
        let migrated = parse_credential_records(&encoded).unwrap();
        assert_eq!(
            migrated
                .into_iter()
                .map(|record| (record.token_hash, record.package_name))
                .collect::<BTreeSet<_>>(),
            legacy
                .into_iter()
                .map(|record| (record.token_hash, record.package_name))
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn migration_preserves_the_maximum_bounded_legacy_acl() {
        let developer = format!("d{}", "x".repeat(63));
        let mut legacy = Vec::with_capacity(MAX_GOVERNANCE_RECORDS);
        for index in 0..MAX_GOVERNANCE_RECORDS {
            let package_name = format!("p{index:04}{}", "x".repeat(59));
            assert_eq!(package_name.len(), 64);
            let mut token_hash = [0u8; 32];
            token_hash[..8].copy_from_slice(&(index as u64).to_be_bytes());
            legacy.push(CredentialRecord {
                token_hash,
                package_name,
            });
        }

        let state = migrate_legacy_credentials(legacy.clone(), &developer);
        assert_eq!(state.audits.len(), 2 + 2 * MAX_GOVERNANCE_RECORDS);
        let encoded = serialize_governance_state(&state)
            .expect("the maximum accepted legacy ACL must fit the governance snapshot bound");
        assert!(encoded.len() > 1024 * 1024);
        assert!(encoded.len() as u64 <= MAX_CREDENTIAL_FILE_BYTES);

        let migrated = parse_credential_records(&encoded)
            .expect("the maximum migrated snapshot must remain replayable");
        let expected = legacy
            .iter()
            .map(|record| (record.token_hash, record.package_name.as_str()))
            .collect::<BTreeSet<_>>();
        let actual = migrated
            .iter()
            .map(|record| (record.token_hash, record.package_name.as_str()))
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn governance_transfer_revokes_old_owner_and_audit_tampering_is_rejected() {
        let (mut state, alice_hash, bob_hash, _) = governance_fixture();
        state.owners.insert(
            "math".to_string(),
            GovernanceOwner::Developer("bob".to_string()),
        );
        append_governance_audit(&mut state, "package-transfer", "math:developer:bob");
        let encoded = serialize_governance_state(&state).unwrap();
        let credentials = parse_credential_records(&encoded).unwrap();
        assert!(!credentials.iter().any(|credential| {
            credential.token_hash == alice_hash && credential.package_name == "math"
        }));
        assert!(credentials.iter().any(|credential| {
            credential.token_hash == bob_hash && credential.package_name == "math"
        }));

        let tampered = String::from_utf8(encoded)
            .unwrap()
            .replace("math:developer:bob", "math:developer:eve");
        let error = parse_governance_state(tampered.as_bytes())
            .expect_err("changing an audit subject must break the hash chain");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_audit"));
    }

    #[test]
    fn governance_audit_binds_owner_member_and_token_state() {
        let (state, _, _, outsider_hash) = governance_fixture();
        let encoded = String::from_utf8(serialize_governance_state(&state).unwrap()).unwrap();
        let token_line = format!("token sha256-{} outsider all", encode_hex(&outsider_hash));
        let mutations = [
            encoded.replacen(
                "owner math developer:alice",
                "owner math developer:outsider",
                1,
            ),
            encoded.replacen("member core bob", "member core outsider", 1),
            encoded.replacen(
                &token_line,
                &token_line.replace(" outsider all", " alice all"),
                1,
            ),
        ];
        for tampered in mutations {
            assert_ne!(tampered, encoded);
            let error = parse_governance_state(tampered.as_bytes())
                .expect_err("governance state changes require a new audit event");
            assert_eq!(error.code.as_deref(), Some("invalid_registry_audit"));
        }
    }

    #[test]
    fn governance_audit_replay_rejects_unknown_fictitious_and_noop_events() {
        let (state, _, bob_hash, _) = governance_fixture();
        let encoded = String::from_utf8(serialize_governance_state(&state).unwrap()).unwrap();
        let previous = state.audits.last().unwrap();
        for (action, subject) in [
            ("unknown-action", "registry".to_string()),
            ("developer-create", "mallory".to_string()),
            ("developer-create", "alice".to_string()),
            ("package-transfer", "math:developer:bob".to_string()),
            (
                "developer-token-revoke",
                format!("bob:sha256-{}", encode_hex(&bob_hash)),
            ),
        ] {
            let sequence = previous.sequence + 1;
            let before = previous.transition_hash.clone();
            let after = before.clone();
            let event_hash = governance_audit_hash(
                sequence,
                &previous.event_hash,
                &before,
                &after,
                action,
                &subject,
            );
            let forged = format!(
                "{encoded}audit {sequence} {} {event_hash} {before} {after} {action} {subject}\n",
                previous.event_hash
            );
            let error = parse_governance_state(forged.as_bytes())
                .expect_err("a forged or no-op audit event must fail semantic replay");
            assert_eq!(error.code.as_deref(), Some("invalid_registry_audit"));
        }
    }

    #[test]
    fn governance_audit_phase_rejects_fictitious_bootstrap_and_scope_transitions() {
        let mut state = GovernanceState::default();
        let mut phase = GovernanceAuditPhase::Empty;
        assert!(
            apply_governance_audit_event(&mut state, &mut phase, "developer-create", "alice")
                .is_err()
        );
        assert!(state.developers.is_empty());

        apply_governance_audit_event(&mut state, &mut phase, "migration-init", "alice").unwrap();
        let all_token = GovernanceToken {
            token_hash: [1u8; 32],
            developer: "alice".to_string(),
            scope: GovernanceTokenScope::All,
        };
        assert!(apply_governance_audit_event(
            &mut state,
            &mut phase,
            "migration-token-import",
            &token_audit_subject(&all_token)
        )
        .is_err());
        let package_token = GovernanceToken {
            token_hash: [2u8; 32],
            developer: "alice".to_string(),
            scope: GovernanceTokenScope::Package("math".to_string()),
        };
        assert!(apply_governance_audit_event(
            &mut state,
            &mut phase,
            "developer-token-issue",
            &token_audit_subject(&package_token)
        )
        .is_err());
        apply_governance_audit_event(
            &mut state,
            &mut phase,
            "migration-token-import",
            &token_audit_subject(&package_token),
        )
        .unwrap();
        assert!(apply_governance_audit_event(
            &mut state,
            &mut phase,
            "migration-complete",
            "alice"
        )
        .is_err());
        apply_governance_audit_event(
            &mut state,
            &mut phase,
            "package-claim",
            "math:developer:alice",
        )
        .unwrap();
        apply_governance_audit_event(&mut state, &mut phase, "migration-complete", "alice")
            .unwrap();
        assert_eq!(phase, GovernanceAuditPhase::Normal);
        assert!(apply_governance_audit_event(
            &mut state,
            &mut phase,
            "migration-token-import",
            &token_audit_subject(&GovernanceToken {
                token_hash: [3u8; 32],
                developer: "alice".to_string(),
                scope: GovernanceTokenScope::Package("math".to_string()),
            })
        )
        .is_err());
    }

    #[test]
    fn governance_audit_rejects_noncanonical_hash_encodings() {
        let token_hash = [0xabu8; 32];
        let uppercase_hash = encode_hex(&token_hash).to_ascii_uppercase();
        let token_subject = format!("alice:sha256-{uppercase_hash}:all");
        let token_error = parse_audit_token_subject(&token_subject)
            .expect_err("audit token subjects must use canonical lowercase hex");
        assert_eq!(token_error.code.as_deref(), Some("invalid_registry_audit"));

        let revoke_subject = format!("alice:sha256-{uppercase_hash}");
        let revoke_error = parse_audit_revoke_subject(&revoke_subject)
            .expect_err("audit revoke subjects must use canonical lowercase hex");
        assert_eq!(revoke_error.code.as_deref(), Some("invalid_registry_audit"));

        let (state, _, _, _) = governance_fixture();
        let encoded = String::from_utf8(serialize_governance_state(&state).unwrap()).unwrap();
        let mut changed = false;
        let noncanonical = encoded
            .lines()
            .map(|line| {
                if !changed && line.starts_with("audit ") {
                    let mut fields = line
                        .split_ascii_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    fields[3] = fields[3].to_ascii_uppercase();
                    changed = true;
                    fields.join(" ")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(changed);
        let error = parse_governance_state(noncanonical.as_bytes())
            .expect_err("governance audit hash fields must use canonical lowercase hex");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_audit"));

        let noncanonical_sequence = encoded.replacen("audit 1 ", "audit 01 ", 1);
        assert_ne!(noncanonical_sequence, encoded);
        let error = parse_governance_state(noncanonical_sequence.as_bytes())
            .expect_err("audit sequence text must use canonical unsigned decimal encoding");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_audit"));

        let mut changed_token = false;
        let noncanonical_token = encoded
            .lines()
            .map(|line| {
                if !changed_token && line.starts_with("token sha256-") {
                    let mut fields = line
                        .split_ascii_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    let hash = fields[1].strip_prefix("sha256-").unwrap();
                    fields[1] = format!("sha256-{}", hash.to_ascii_uppercase());
                    changed_token = true;
                    fields.join(" ")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(changed_token);
        let error = parse_governance_state(noncanonical_token.as_bytes())
            .expect_err("governance token hashes must use canonical lowercase hex");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_credentials"));
    }

    #[test]
    fn governance_acl_expansion_scales_with_effective_records_not_token_owner_product() {
        let mut state = GovernanceState::default();
        for index in 0..MAX_GOVERNANCE_RECORDS {
            let developer = format!("d{index:04}");
            let package = format!("p{index:04}");
            let mut token_hash = [0u8; 32];
            token_hash[..8].copy_from_slice(&(index as u64).to_be_bytes());
            state.developers.insert(developer.clone());
            state
                .owners
                .insert(package, GovernanceOwner::Developer(developer.clone()));
            state.tokens.insert(GovernanceToken {
                token_hash,
                developer,
                scope: GovernanceTokenScope::All,
            });
        }
        let credentials = governance_credentials(&state).unwrap();
        assert_eq!(credentials.len(), MAX_GOVERNANCE_RECORDS);
        assert!(all_packages_remain_authorized(&state));
        assert_eq!(
            credentials
                .iter()
                .map(|record| (&record.token_hash, &record.package_name))
                .collect::<BTreeSet<_>>()
                .len(),
            MAX_GOVERNANCE_RECORDS
        );
    }

    #[test]
    fn governance_team_acl_rejects_before_materializing_team_token_product() {
        let mut state = GovernanceState::default();
        let developer = "alice".to_string();
        state.developers.insert(developer.clone());
        for index in 0..MAX_GOVERNANCE_RECORDS {
            let team = format!("t{index:04}");
            let package = format!("p{index:04}");
            state.teams.insert(team.clone());
            state.members.insert((team.clone(), developer.clone()));
            state.owners.insert(package, GovernanceOwner::Team(team));

            let mut token_hash = [0u8; 32];
            token_hash[..8].copy_from_slice(&(index as u64).to_be_bytes());
            state.tokens.insert(GovernanceToken {
                token_hash,
                developer: developer.clone(),
                scope: GovernanceTokenScope::All,
            });
        }

        let error = governance_credentials(&state)
            .expect_err("team ACL expansion must stop at the effective record cap");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_governance"));
    }

    #[test]
    fn governance_team_acl_does_not_rescan_members_without_all_tokens_per_owner() {
        let mut state = GovernanceState::default();
        let team = "core".to_string();
        state.teams.insert(team.clone());
        for index in 0..MAX_GOVERNANCE_RECORDS {
            let developer = format!("d{index:04}");
            let package = format!("p{index:04}");
            state.developers.insert(developer.clone());
            state.members.insert((team.clone(), developer.clone()));
            state
                .owners
                .insert(package, GovernanceOwner::Team(team.clone()));
        }
        let mut token_hash = [0u8; 32];
        token_hash[..8].copy_from_slice(&1u64.to_be_bytes());
        state.tokens.insert(GovernanceToken {
            token_hash,
            developer: "d0000".to_string(),
            scope: GovernanceTokenScope::Package("p0000".to_string()),
        });

        let indexes = governance_credential_indexes(&state);
        assert_eq!(
            indexes.members_by_team.get("core").map(HashSet::len),
            Some(MAX_GOVERNANCE_RECORDS)
        );
        assert!(
            !indexes.all_token_members_by_team.contains_key("core"),
            "members without all-scoped tokens must not enter the per-owner iteration index"
        );
        let credentials = governance_credentials(&state).unwrap();
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].token_hash, token_hash);
        assert_eq!(credentials[0].package_name, "p0000");
    }

    #[test]
    fn governance_rejects_an_owner_without_an_effective_token() {
        let (mut state, alice_hash, _, _) = governance_fixture();
        state.tokens.retain(|token| token.token_hash != alice_hash);
        let error = validate_governance_state(&state)
            .expect_err("an owner without a publishing credential is not a valid snapshot");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_governance"));
    }

    #[test]
    fn governance_rejects_duplicate_token_identity_and_dangling_references() {
        let (mut duplicate, alice_hash, _, _) = governance_fixture();
        duplicate.tokens.insert(GovernanceToken {
            token_hash: alice_hash,
            developer: "bob".to_string(),
            scope: GovernanceTokenScope::All,
        });
        let error = serialize_governance_state(&duplicate)
            .expect_err("one bearer token cannot authenticate two identities");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_governance"));

        let (mut dangling, _, _, _) = governance_fixture();
        dangling
            .members
            .insert(("missing".to_string(), "alice".to_string()));
        assert!(serialize_governance_state(&dangling).is_err());
    }

    #[test]
    fn issue_and_revoke_preserve_compatible_credentials() {
        let root = TestDirectory::new("roundtrip");
        let path = root.0.join("credentials.txt");
        let old_hash: [u8; 32] = Sha256::digest(b"old-token").into();
        fs::write(
            &path,
            format!("# existing\r\nsha256-{} other\r\n", encode_hex(&old_hash)),
        )
        .unwrap();
        let token = issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        assert!(token.starts_with("ku_"));
        assert_eq!(token.len(), 3 + TOKEN_RANDOM_BYTES * 2);
        let issued = fs::read(&path).unwrap();
        assert!(issued.starts_with(b"# existing\r\n"));
        let records = parse_credential_records(&issued).unwrap();
        assert_eq!(records.len(), 2);
        revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1)).unwrap();
        let revoked = fs::read(&path).unwrap();
        assert_eq!(parse_credential_records(&revoked).unwrap().len(), 1);
        assert!(String::from_utf8(revoked).unwrap().contains("# existing"));
    }

    #[test]
    fn revoking_the_last_credential_fails_without_rewriting_the_file() {
        let root = TestDirectory::new("last-credential");
        let path = root.0.join("credentials.txt");
        let token = "only-token";
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let original = format!("# keep\nsha256-{} math\n", encode_hex(&hash));
        fs::write(&path, original.as_bytes()).unwrap();
        let error = revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1))
            .expect_err("the last service credential must remain valid");
        assert_eq!(error.code.as_deref(), Some("registry_last_credential"));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    }

    #[test]
    fn atomic_crash_boundaries_leave_a_readable_old_or_new_file() {
        let root = TestDirectory::new("crash-boundaries");
        let path = root.0.join("credentials.txt");
        let old_hash: [u8; 32] = Sha256::digest(b"old").into();
        let new_hash: [u8; 32] = Sha256::digest(b"new").into();
        let old = format!("sha256-{} math\n", encode_hex(&old_hash));
        let new = format!("sha256-{} math\n", encode_hex(&new_hash));
        fs::write(&path, old.as_bytes()).unwrap();

        let before = write_credentials_atomically_with_hook(&path, new.as_bytes(), |point| {
            if point == AtomicWriteCheckpoint::BeforeReplace {
                Err(io::Error::other("simulated crash"))
            } else {
                Ok(())
            }
        });
        assert!(before.is_err());
        assert_eq!(fs::read(&path).unwrap(), old.as_bytes());
        parse_credential_records(&fs::read(&path).unwrap()).unwrap();

        let after = write_credentials_atomically_with_hook(&path, new.as_bytes(), |point| {
            if point == AtomicWriteCheckpoint::AfterReplace {
                Err(io::Error::other("simulated crash"))
            } else {
                Ok(())
            }
        });
        assert!(after.is_err());
        assert_eq!(fs::read(&path).unwrap(), new.as_bytes());
        parse_credential_records(&fs::read(&path).unwrap()).unwrap();
    }

    #[test]
    fn committed_issue_and_revoke_errors_report_exact_hash_without_plaintext() {
        let root = TestDirectory::new("commit-error-recovery");
        let path = root.0.join("credentials.txt");
        let old_token = "old-plaintext-must-not-appear-in-errors";
        let old_hash: [u8; 32] = Sha256::digest(old_token.as_bytes()).into();
        let old = format!("sha256-{} math\n", encode_hex(&old_hash));
        fs::write(&path, old.as_bytes()).unwrap();

        let error =
            issue_token_with_writer(&path, "math", Duration::from_secs(1), |path, bytes| {
                write_credentials_atomically_with_hooks(
                    path,
                    bytes,
                    |_| Ok(()),
                    |_| Err(io::Error::other("simulated directory fsync failure")),
                )
            })
            .expect_err("a post-commit fsync error must not return a successful token");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_commit_uncertain")
        );
        let committed = parse_credential_records(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            committed.len(),
            2,
            "the issue was committed despite fsync failure"
        );
        let issued_hash = committed
            .iter()
            .find(|record| record.token_hash != old_hash)
            .expect("locate newly committed hash")
            .token_hash;
        assert!(error.message.contains(&format!(
            "issued credential sha256-{}",
            encode_hex(&issued_hash)
        )));
        assert!(error.message.contains("for package math"));
        assert!(error.message.contains("durability is uncertain"));
        assert!(!error.message.contains(old_token));
        assert!(
            !error.message.contains("ku_"),
            "no generated plaintext may enter diagnostics"
        );
        assert!(!staging_path(&path).unwrap().exists());

        let error = revoke_token_with_writer(
            &path,
            "math",
            old_token.as_bytes(),
            Duration::from_secs(1),
            |path, bytes| {
                write_credentials_atomically_with_hook(path, bytes, |point| {
                    if point == AtomicWriteCheckpoint::AfterReplace {
                        Err(io::Error::other("simulated post-replacement failure"))
                    } else {
                        Ok(())
                    }
                })
            },
        )
        .expect_err("a post-commit revoke error must identify the committed mutation");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_commit_uncertain")
        );
        assert!(error.message.contains(&format!(
            "revoked credential sha256-{}",
            encode_hex(&old_hash)
        )));
        assert!(error.message.contains("for package math"));
        assert!(!error.message.contains(old_token));
        let after_revoke = fs::read(&path).unwrap();
        let remaining = parse_credential_records(&after_revoke).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].token_hash, issued_hash);
        let retry = revoke_token(&path, "math", old_token.as_bytes(), Duration::from_secs(1))
            .expect_err("retrying an already committed revocation has no matching credential");
        assert_eq!(retry.code.as_deref(), Some("registry_credential_not_found"));
        assert!(retry.message.contains("already have been revoked"));
        assert_eq!(fs::read(&path).unwrap(), after_revoke);

        let developer_error = annotate_committed_credential_error(
            admin_error(
                "registry_credentials_commit_uncertain",
                "replacement is visible",
            ),
            "issued",
            "developer",
            "alice",
            &issued_hash,
        );
        assert!(developer_error.message.contains("for developer alice"));
        assert!(!developer_error.message.contains("for package alice"));
    }

    #[test]
    fn token_and_revocation_output_failures_are_committed_and_redacted() {
        struct FailingOutput<'a> {
            fail_write: bool,
            private_error: &'a str,
            written: Vec<u8>,
        }
        impl Write for FailingOutput<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.fail_write {
                    Err(io::Error::other(self.private_error.to_string()))
                } else {
                    self.written.extend_from_slice(bytes);
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other(self.private_error.to_string()))
            }
        }
        let token = format!("ku_{}", "a".repeat(64));
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        for fail_write in [true, false] {
            let mut output = FailingOutput {
                fail_write,
                private_error: &token,
                written: Vec::new(),
            };
            let error =
                write_issued_token(&token, &mut output).expect_err("injected output failure");
            assert_eq!(error.code.as_deref(), Some("registry_token_output_failed"));
            assert!(error
                .message
                .contains(&format!("sha256-{}", encode_hex(&hash))));
            assert!(error.message.contains("committed"));
            assert!(
                !error.to_string().contains(&token),
                "even a writer's private error must be redacted"
            );
            if fail_write {
                assert!(output.written.is_empty());
            } else {
                assert_eq!(output.written, format!("{token}\n").as_bytes());
            }
            let error = write_revocation_confirmation("math", &mut output)
                .expect_err("revoke confirmation failure must be structured, not a panic");
            assert_eq!(
                error.code.as_deref(),
                Some("registry_revocation_output_failed")
            );
            assert!(error.message.contains("committed"));
            assert!(!error.to_string().contains(&token));

            let error = write_developer_issued_token("alice", &token, &mut output)
                .expect_err("developer token output failure must expose hash recovery only");
            assert_eq!(error.code.as_deref(), Some("registry_token_output_failed"));
            let hash_text = format!("sha256-{}", encode_hex(&hash));
            assert!(error.message.contains(&hash_text));
            assert!(error
                .message
                .contains(&format!("developer token-revoke-hash alice {hash_text}")));
            assert!(error.message.contains("committed"));
            assert!(!error.to_string().contains(&token));

            let error = write_governance_confirmation("created developer alice", &mut output)
                .expect_err("governance confirmation failure must be structured");
            assert_eq!(
                error.code.as_deref(),
                Some("registry_governance_output_failed")
            );
            assert!(error.message.contains("committed"));
            assert!(error.message.contains("already-exists or already-applied"));
            assert!(!error.to_string().contains(&token));

            let error = write_audit_verification_confirmation(&mut output)
                .expect_err("audit output failure must be structured");
            assert_eq!(error.code.as_deref(), Some("registry_audit_output_failed"));
            assert!(error.message.contains("verification succeeded"));
            assert!(!error.message.contains("committed"));
            assert!(!error.to_string().contains(&token));
        }
        let mut success = Vec::new();
        write_issued_token(&token, &mut success).unwrap();
        assert_eq!(success, format!("{token}\n").as_bytes());

        let mut success = Vec::new();
        write_developer_issued_token("alice", &token, &mut success).unwrap();
        assert_eq!(success, format!("{token}\n").as_bytes());
    }

    #[cfg(windows)]
    #[test]
    fn windows_existing_dacl_is_preserved_before_writing_and_after_issue_revoke() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = TestDirectory::new("windows-preserve-dacl");
        let path = root.0.join("credentials.txt");
        let hash: [u8; 32] = Sha256::digest(b"original").into();
        let original = format!("sha256-{} original\n", encode_hex(&hash));
        fs::write(&path, original.as_bytes()).unwrap();
        let private = credential_dacl::Dacl::from_sddl("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)")
            .expect("create private DACL without symlink privilege");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(0x1000_0000)
            .open(&path)
            .expect("open original with security access");
        private.apply(&file).expect("install private original DACL");
        let expected = credential_dacl::Dacl::read(&file)
            .unwrap()
            .snapshot()
            .unwrap();
        drop(file);
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |point| {
            if point == AtomicWriteCheckpoint::AfterPermissions {
                let staging = open_nofollow(&staging_path(&path).unwrap(), false, false).unwrap();
                assert_eq!(
                    staging.metadata().unwrap().len(),
                    0,
                    "copy DACL before writing credential hashes"
                );
                assert_eq!(
                    credential_dacl::Dacl::read(&staging)
                        .unwrap()
                        .snapshot()
                        .unwrap(),
                    expected
                );
                Err(io::Error::other("stop after permission verification"))
            } else {
                Ok(())
            }
        })
        .expect_err("injected pre-write stop");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_write_failed")
        );
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
        let token = issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        let current = open_nofollow(&path, false, false).unwrap();
        assert_eq!(
            credential_dacl::Dacl::read(&current)
                .unwrap()
                .snapshot()
                .unwrap(),
            expected
        );
        drop(current);
        revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1)).unwrap();
        let current = open_nofollow(&path, false, false).unwrap();
        assert_eq!(
            credential_dacl::Dacl::read(&current)
                .unwrap()
                .snapshot()
                .unwrap(),
            expected
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_foreign_owner_group_are_preserved_or_fail_before_content_write() {
        let root = TestDirectory::new("windows-foreign-owner-group");
        let staging_path = root.0.join("staging.txt");
        let staging = open_new_private_file(&staging_path).unwrap();
        let foreign = credential_dacl::Dacl::from_sddl("O:SYG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)")
            .expect("create a descriptor with explicit non-operator owner and group");
        let expected = foreign.snapshot().unwrap();
        match foreign.apply(&staging) {
            Ok(()) => assert_eq!(
                credential_dacl::Dacl::read(&staging)
                    .unwrap()
                    .snapshot()
                    .unwrap(),
                expected
            ),
            Err(_) => assert_eq!(
                staging.metadata().unwrap().len(),
                0,
                "an unprivileged owner/group failure must happen before credential content is written"
            ),
        }
    }

    fn linux_acl_bytes(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in entries {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&permissions.to_le_bytes());
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn linux_mode_only_acl_parser_rejects_extended_and_unknown_encodings() {
        for owner in 0..8 {
            for group in 0..8 {
                for other in 0..8 {
                    assert!(linux_access_acl_is_mode_only(&linux_acl_bytes(&[
                        (1, owner, u32::MAX),
                        (4, group, u32::MAX),
                        (32, other, u32::MAX),
                    ])));
                }
            }
        }
        let valid = linux_acl_bytes(&[(1, 6, u32::MAX), (4, 4, u32::MAX), (32, 0, u32::MAX)]);
        for length in 0..valid.len() {
            assert!(!linux_access_acl_is_mode_only(&valid[..length]));
        }
        let mut wrong_version = valid.clone();
        wrong_version[0] = 3;
        assert!(!linux_access_acl_is_mode_only(&wrong_version));
        for tag in [0u16, 2, 8, 16, 33] {
            let mut named_or_unknown = valid.clone();
            named_or_unknown[12..14].copy_from_slice(&tag.to_le_bytes());
            assert!(!linux_access_acl_is_mode_only(&named_or_unknown));
        }
        let mut bad_permissions = valid.clone();
        bad_permissions[6..8].copy_from_slice(&8u16.to_le_bytes());
        assert!(!linux_access_acl_is_mode_only(&bad_permissions));
        let mut named_id = valid.clone();
        named_id[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(!linux_access_acl_is_mode_only(&named_id));
        let mut trailing_entry = valid.clone();
        trailing_entry.extend_from_slice(&[0; 8]);
        assert!(!linux_access_acl_is_mode_only(&trailing_entry));
        let out_of_order =
            linux_acl_bytes(&[(4, 4, u32::MAX), (1, 6, u32::MAX), (32, 0, u32::MAX)]);
        assert!(!linux_access_acl_is_mode_only(&out_of_order));
    }

    #[test]
    fn macos_empty_acl_header_rejects_entries_flags_and_unknown_encodings() {
        let mut header = [0u32; 11];
        header[0] = 0x012c_c16du32.to_be();
        assert!(macos_extended_acl_header_is_mode_only(&header, 44));
        for size in [-1, 0, 43, 45, 68] {
            assert!(!macos_extended_acl_header_is_mode_only(&header, size));
        }
        for flags in [1u32, 0xffff, 1 << 16, 1 << 17, 1 << 31, u32::MAX] {
            header[10] = flags.to_be();
            assert!(!macos_extended_acl_header_is_mode_only(&header, 44));
        }
        header[10] = 0;
        header[9] = 1u32.to_be();
        assert!(!macos_extended_acl_header_is_mode_only(&header, 44));
        header[9] = 0;
        header[0] = 0;
        assert!(!macos_extended_acl_header_is_mode_only(&header, 44));
    }

    #[cfg(target_os = "linux")]
    fn install_linux_test_acl(file: &fs::File, default_acl: bool) -> Option<Vec<u8>> {
        use std::os::unix::io::AsRawFd;
        let named_uid = if unsafe { libc::geteuid() } == 1 {
            2
        } else {
            1
        };
        let value = linux_acl_bytes(&[
            (1, 7, u32::MAX),
            (2, 4, named_uid),
            (4, 0, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ]);
        let name = if default_acl {
            c"system.posix_acl_default"
        } else {
            c"system.posix_acl_access"
        };
        // SAFETY: the test owns this file/dir fd and the serialized ACL buffer.
        if unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOTSUP) {
                eprintln!(
                    "skip extended ACL fixture: temporary filesystem has no POSIX ACL support"
                );
                return None;
            }
            panic!("install real Linux ACL fixture: {error}");
        }
        Some(value)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_source_extended_acl_is_rejected_without_replacing_credentials() {
        use std::os::unix::io::AsRawFd;
        let root = TestDirectory::new("linux-source-acl");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let file = fs::File::open(&path).unwrap();
        let Some(expected_acl) = install_linux_test_acl(&file, false) else {
            return;
        };
        let mut reached_write = false;
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |_| {
            reached_write = true;
            Ok(())
        })
        .expect_err("source extended ACL must not be silently discarded");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert!(error.message.contains("original"));
        assert!(!reached_write, "ACL rejection must precede staging writes");
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
        let mut actual_acl = [0u8; 44];
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"system.posix_acl_access".as_ptr(),
                actual_acl.as_mut_ptr().cast(),
                actual_acl.len(),
            )
        };
        assert_eq!(size, actual_acl.len() as isize);
        assert_eq!(actual_acl.as_slice(), expected_acl.as_slice());
        let error = issue_token(&path, "math", Duration::from_secs(1)).unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_inherited_staging_acl_is_rejected_for_existing_and_first_credentials() {
        let root = TestDirectory::new("linux-default-acl");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        // Create the original before installing the parent's default ACL, so
        // this test reaches the separate staging-fd check.
        fs::write(&path, original.as_bytes()).unwrap();
        let parent = fs::File::open(&root.0).unwrap();
        if install_linux_test_acl(&parent, true).is_none() {
            return;
        }
        let original_file = fs::File::open(&path).unwrap();
        ensure_credential_file_has_no_extended_acl(&original_file, "original").unwrap();
        let probe_path = root.0.join("empty-probe");
        let probe = open_new_private_file(&probe_path).unwrap();
        assert_eq!(probe.metadata().unwrap().len(), 0);
        assert_eq!(
            ensure_credential_file_has_no_extended_acl(&probe, "staging")
                .unwrap_err()
                .code
                .as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        drop(probe);
        fs::remove_file(&probe_path).unwrap();
        for destination in [&path, &root.0.join("first-credentials.txt")] {
            let mut reached_write = false;
            let error =
                write_credentials_atomically_with_hook(destination, original.as_bytes(), |_| {
                    reached_write = true;
                    Ok(())
                })
                .expect_err("inherited ACL must be rejected before any bytes are written");
            assert_eq!(
                error.code.as_deref(),
                Some("registry_credentials_acl_unsupported")
            );
            assert!(error.message.contains("staging"));
            assert!(!reached_write);
            assert!(!staging_path(destination).unwrap().exists());
        }
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!root.0.join("first-credentials.txt").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_source_extended_acl_is_rejected_without_replacing_credentials() {
        let root = TestDirectory::new("macos-source-acl");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let file = fs::File::open(&path).unwrap();
        credential_macos_acl::install_test_acl(
            &file,
            c"!#acl 1\nuser:00000000-0000-0000-0000-000000000001:::allow:read\n",
        )
        .expect("install real extended macOS ACL without account lookup");
        let mut reached_write = false;
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |_| {
            reached_write = true;
            Ok(())
        })
        .expect_err("source extended ACL must not be silently discarded");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert!(error.message.contains("original"));
        assert!(!reached_write);
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
        let error = issue_token(&path, "math", Duration::from_secs(1)).unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_inherited_staging_acl_is_rejected_for_existing_and_first_credentials() {
        let root = TestDirectory::new("macos-inherit-acl");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let parent = fs::File::open(&root.0).unwrap();
        credential_macos_acl::install_test_acl(
            &parent,
            c"!#acl 1\nuser:00000000-0000-0000-0000-000000000001:::allow,file_inherit,only_inherit:read\n",
        ).expect("install inheritable macOS ACL");
        let original_file = fs::File::open(&path).unwrap();
        ensure_credential_file_has_no_extended_acl(&original_file, "original").unwrap();
        let probe_path = root.0.join("empty-probe");
        let probe = open_new_private_file(&probe_path).unwrap();
        assert_eq!(probe.metadata().unwrap().len(), 0);
        assert_eq!(
            ensure_credential_file_has_no_extended_acl(&probe, "staging")
                .unwrap_err()
                .code
                .as_deref(),
            Some("registry_credentials_acl_unsupported")
        );
        drop(probe);
        fs::remove_file(&probe_path).unwrap();
        for destination in [&path, &root.0.join("first-credentials.txt")] {
            let mut reached_write = false;
            let error =
                write_credentials_atomically_with_hook(destination, original.as_bytes(), |_| {
                    reached_write = true;
                    Ok(())
                })
                .expect_err("inherited ACL must be rejected before any bytes are written");
            assert_eq!(
                error.code.as_deref(),
                Some("registry_credentials_acl_unsupported")
            );
            assert!(error.message.contains("staging"));
            assert!(!reached_write);
            assert!(!staging_path(destination).unwrap().exists());
        }
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!root.0.join("first-credentials.txt").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_explicit_empty_acl_allows_issue_and_revoke() {
        let root = TestDirectory::new("macos-empty-acl");
        let path = root.0.join("credentials.txt");
        fs::write(
            &path,
            format!(
                "sha256-{} original\n",
                encode_hex(&Sha256::digest(b"original"))
            ),
        )
        .unwrap();
        let file = fs::File::open(&path).unwrap();
        credential_macos_acl::install_test_acl(&file, c"!#acl 1\n")
            .expect("install explicitly empty macOS ACL");
        ensure_credential_file_has_no_extended_acl(&file, "original").unwrap();
        let token = issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_existing_uid_gid_and_mode_are_preserved() {
        use std::os::unix::{
            fs::{MetadataExt, PermissionsExt},
            io::AsRawFd,
        };
        let root = TestDirectory::new("unix-preserve-owner-mode");
        let path = root.0.join("credentials.txt");
        let hash: [u8; 32] = Sha256::digest(b"original").into();
        let original = format!("sha256-{} original\n", encode_hex(&hash));
        fs::write(&path, original.as_bytes()).unwrap();
        // Root-capable runs exercise a genuinely different owner and group;
        // ordinary CI users verify their existing uid/gid without privilege.
        if unsafe { libc::geteuid() } == 0 {
            let file = fs::File::open(&path).unwrap();
            assert_eq!(unsafe { libc::fchown(file.as_raw_fd(), 65_534, 65_534) }, 0);
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::metadata(&path).unwrap();
        let expected = (before.uid(), before.gid(), before.mode() & 0o7777);
        let error = write_credentials_atomically_with_hook(&path, original.as_bytes(), |point| {
            if point == AtomicWriteCheckpoint::AfterPermissions {
                let staging = fs::metadata(staging_path(&path).unwrap()).unwrap();
                assert_eq!(staging.len(), 0);
                assert_eq!(
                    (staging.uid(), staging.gid(), staging.mode() & 0o7777),
                    expected
                );
                Err(io::Error::other("stop before credential bytes are written"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_write_failed")
        );
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        let token = issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        let issued = fs::metadata(&path).unwrap();
        assert_eq!(
            (issued.uid(), issued.gid(), issued.mode() & 0o7777),
            expected
        );
        revoke_token(&path, "math", token.as_bytes(), Duration::from_secs(1)).unwrap();
        let revoked = fs::metadata(&path).unwrap();
        assert_eq!(
            (revoked.uid(), revoked.gid(), revoked.mode() & 0o7777),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_permission_copy_and_parent_open_failures_stop_before_commit() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skip unprivileged ownership/directory access failures under root");
            return;
        }
        let root = TestDirectory::new("unix-precommit-access-failure");
        let path = root.0.join("credentials.txt");
        let original = format!(
            "sha256-{} original\n",
            encode_hex(&Sha256::digest(b"original"))
        );
        fs::write(&path, original.as_bytes()).unwrap();
        let staging = open_new_private_file(&staging_path(&path).unwrap()).unwrap();
        let metadata = staging.metadata().unwrap();
        let foreign = CredentialFilePermissions {
            uid: if metadata.uid() == 1 { 2 } else { 1 },
            gid: metadata.gid(),
            mode: 0o600,
        };
        let error = foreign
            .apply(&staging)
            .expect_err("unprivileged owner change must fail");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_permissions_failed")
        );
        assert_eq!(staging.metadata().unwrap().len(), 0);
        drop(staging);
        fs::remove_file(staging_path(&path).unwrap()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());

        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o300)).unwrap();
        let result = issue_token(&path, "math", Duration::from_secs(1));
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.expect_err("directory read-open denial must precede replacement");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_write_failed")
        );
        assert!(error.message.contains("before replacement"));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(!staging_path(&path).unwrap().exists());
    }

    const CRASH_FIXTURE_MODE: &str = "KU_TEST_REGISTRY_ADMIN_CRASH_STAGE";
    const CRASH_FIXTURE_PATH: &str = "KU_TEST_REGISTRY_ADMIN_CRASH_PATH";
    const CRASH_FIXTURE_EXIT_CODE: i32 = 86;

    #[test]
    fn atomic_crash_fixture_child() {
        let Ok(mode) = env::var(CRASH_FIXTURE_MODE) else {
            return;
        };
        let path =
            PathBuf::from(env::var_os(CRASH_FIXTURE_PATH).expect("crash fixture credentials path"));
        let new_hash: [u8; 32] = Sha256::digest(b"crash-new").into();
        let replacement = format!("sha256-{} math\n", encode_hex(&new_hash));
        write_credentials_atomically_with_hook(&path, replacement.as_bytes(), |point| {
            let should_exit = matches!(
                (mode.as_str(), point),
                ("before", AtomicWriteCheckpoint::BeforeReplace)
                    | ("after", AtomicWriteCheckpoint::AfterReplace)
            );
            if should_exit {
                // This boundary needs a real process stop without Rust stack
                // unwinding. A non-zero exit preserves that property while
                // avoiding OS crash-reporting/debugger latency from abort().
                std::process::exit(CRASH_FIXTURE_EXIT_CODE);
            }
            Ok(())
        })
        .expect("crash fixture reaches selected checkpoint");
    }

    #[test]
    fn real_process_crash_before_or_after_replace_keeps_a_parseable_file() {
        let root = TestDirectory::new("real-crash");
        let path = root.0.join("credentials.txt");
        let old_hash: [u8; 32] = Sha256::digest(b"crash-old").into();
        let new_hash: [u8; 32] = Sha256::digest(b"crash-new").into();
        let old = format!("sha256-{} math\n", encode_hex(&old_hash));
        let new = format!("sha256-{} math\n", encode_hex(&new_hash));
        fs::write(&path, old.as_bytes()).unwrap();

        run_crashing_fixture(&path, "before");
        assert_eq!(fs::read(&path).unwrap(), old.as_bytes());
        parse_credential_records(&fs::read(&path).unwrap()).unwrap();
        assert!(staging_path(&path).unwrap().is_file());

        write_credentials_atomically(&path, old.as_bytes())
            .expect("next operation cleans crash staging and preserves old file");
        assert!(!staging_path(&path).unwrap().exists());
        run_crashing_fixture(&path, "after");
        assert_eq!(fs::read(&path).unwrap(), new.as_bytes());
        parse_credential_records(&fs::read(&path).unwrap()).unwrap();
        assert!(!staging_path(&path).unwrap().exists());
    }

    fn run_crashing_fixture(path: &Path, mode: &str) {
        let mut child = Command::new(env::current_exe().expect("current lib test executable"))
            .args([
                "--exact",
                "registry_admin::tests::atomic_crash_fixture_child",
                "--nocapture",
            ])
            .env(CRASH_FIXTURE_MODE, mode)
            .env(CRASH_FIXTURE_PATH, path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn registry admin crash fixture");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait().expect("poll registry admin crash fixture") {
                Some(status) => break status,
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                None => {
                    let _ = child.kill();
                    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
                    loop {
                        match child
                            .try_wait()
                            .expect("reap timed-out registry admin crash fixture")
                        {
                            Some(_) => break,
                            None if Instant::now() < cleanup_deadline => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            None => panic!(
                                "registry admin crash fixture could not be reaped within 2 seconds"
                            ),
                        }
                    }
                    panic!("registry admin crash fixture exceeded its 5 second deadline");
                }
            }
        };
        assert_eq!(
            status.code(),
            Some(CRASH_FIXTURE_EXIT_CODE),
            "crash fixture must reach the selected non-unwinding exit"
        );
    }

    #[test]
    fn staging_failure_does_not_rewrite_the_original_and_stale_file_is_reused() {
        let root = TestDirectory::new("staging-recovery");
        let path = root.0.join("credentials.txt");
        let original_hash: [u8; 32] = Sha256::digest(b"original").into();
        let original = format!("sha256-{} original\n", encode_hex(&original_hash));
        fs::write(&path, original.as_bytes()).unwrap();
        let staging = staging_path(&path).unwrap();
        fs::create_dir(&staging).unwrap();
        let error = issue_token(&path, "math", Duration::from_secs(1))
            .expect_err("non-file staging path must fail closed");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_file"));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());

        fs::remove_dir(&staging).unwrap();
        fs::write(&staging, b"incomplete previous staging bytes").unwrap();
        issue_token(&path, "math", Duration::from_secs(1))
            .expect("a bounded stale regular staging file can be replaced under the lock");
        assert!(!staging.exists());
        assert_eq!(
            parse_credential_records(&fs::read(&path).unwrap())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn lock_wait_has_an_absolute_deadline() {
        let root = TestDirectory::new("lock-deadline");
        let path = normalize_credentials_path(&root.0.join("credentials.txt")).unwrap();
        let held =
            CredentialFileLock::acquire(&path, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = CredentialFileLock::acquire(&path, Instant::now() + Duration::from_millis(50))
            .expect_err("second lock must time out");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_lock_timeout")
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
    }

    #[test]
    fn expired_deadline_rejects_an_immediately_available_lock() {
        let root = TestDirectory::new("expired-free-lock");
        let path = normalize_credentials_path(&root.0.join("credentials.txt")).unwrap();
        let error = CredentialFileLock::acquire(&path, Instant::now())
            .expect_err("an already expired deadline must not acquire a free lock");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_lock_timeout")
        );
        CredentialFileLock::acquire(&path, Instant::now() + Duration::from_secs(1))
            .expect("expired attempt must not leave the file locked");
    }

    #[test]
    fn lock_identity_check_rejects_a_regular_file_replacement() {
        let root = TestDirectory::new("lock-replacement");
        let credentials = normalize_credentials_path(&root.0.join("credentials.txt")).unwrap();
        let lock_path = sibling_lock_path(&credentials).unwrap();
        let opened = open_nofollow(&lock_path, true, true).unwrap();
        let displaced = root.0.join("displaced.lock");
        fs::rename(&lock_path, &displaced).unwrap();
        let replacement = open_nofollow(&lock_path, true, true).unwrap();
        drop(replacement);
        let error = ensure_lock_path_identity(&lock_path, &opened)
            .expect_err("different lock file identities must be rejected");
        assert_eq!(
            error.code.as_deref(),
            Some("registry_credentials_lock_replaced")
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_credentials_and_lock_files_are_private_and_links_are_rejected() {
        use std::os::unix::fs::{symlink, MetadataExt};
        let root = TestDirectory::new("unix-safety");
        let path = root.0.join("credentials.txt");
        issue_token(&path, "math", Duration::from_secs(1)).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(sibling_lock_path(&path).unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );

        let target = root.0.join("target.txt");
        fs::write(&target, b"untouched").unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        assert!(issue_token(&path, "math", Duration::from_secs(1)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");

        let second = TestDirectory::new("unix-lock-link");
        let second_path = second.0.join("credentials.txt");
        let lock_target = second.0.join("lock-target.txt");
        fs::write(&lock_target, b"lock-target-untouched").unwrap();
        symlink(&lock_target, sibling_lock_path(&second_path).unwrap()).unwrap();
        assert!(issue_token(&second_path, "math", Duration::from_secs(1)).is_err());
        assert_eq!(fs::read(&lock_target).unwrap(), b"lock-target-untouched");
    }

    #[cfg(windows)]
    #[test]
    fn windows_credentials_and_lock_reparse_points_are_rejected() {
        use std::os::windows::fs::symlink_file;

        fn create_link_or_skip(target: &Path, link: &Path) -> bool {
            match symlink_file(target, link) {
                Ok(()) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                    ) || error.raw_os_error() == Some(1314) =>
                {
                    eprintln!(
                        "skipping Windows reparse-point assertion because file symlink creation is unavailable: {error}"
                    );
                    false
                }
                Err(error) => panic!("create Windows file symlink: {error}"),
            }
        }

        let root = TestDirectory::new("windows-file-reparse");
        let target = root.0.join("target.txt");
        let credentials = root.0.join("credentials.txt");
        fs::write(&target, b"untouched").unwrap();
        if !create_link_or_skip(&target, &credentials) {
            return;
        }
        assert!(issue_token(&credentials, "math", Duration::from_secs(1)).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");

        let second = TestDirectory::new("windows-lock-reparse");
        let second_credentials = second.0.join("credentials.txt");
        let lock_target = second.0.join("lock-target.txt");
        fs::write(&lock_target, b"lock-target-untouched").unwrap();
        if !create_link_or_skip(
            &lock_target,
            &sibling_lock_path(&second_credentials).unwrap(),
        ) {
            return;
        }
        assert!(issue_token(&second_credentials, "math", Duration::from_secs(1)).is_err());
        assert_eq!(fs::read(&lock_target).unwrap(), b"lock-target-untouched");
    }

    #[test]
    fn governance_parser_stops_on_the_first_developer_over_the_record_limit() {
        let mut bounded = String::from("schema 2\n");
        for index in 0..MAX_GOVERNANCE_RECORDS {
            bounded.push_str(&format!("developer d{index}\n"));
        }

        let mut at_limit = bounded.clone();
        at_limit.push_str(&"x".repeat(MAX_REGISTRY_LINE_BYTES + 1));
        at_limit.push('\n');
        let error = parse_governance_state(at_limit.as_bytes())
            .expect_err("the parser must accept the bounded record count before reading on");
        assert!(error
            .message
            .contains(&format!("line {} is too long", MAX_GOVERNANCE_RECORDS + 2)));

        bounded.push_str(&format!("developer d{}\n", MAX_GOVERNANCE_RECORDS));
        bounded.push_str(&"x".repeat(MAX_REGISTRY_LINE_BYTES + 1));
        bounded.push('\n');
        let error = parse_governance_state(bounded.as_bytes())
            .expect_err("the first excess developer must stop parsing before later input");
        assert_eq!(error.code.as_deref(), Some("invalid_registry_governance"));
        assert!(error.message.contains(&format!(
            "developer record limit {MAX_GOVERNANCE_RECORDS} exceeded at line {}",
            MAX_GOVERNANCE_RECORDS + 2
        )));
        assert!(!error.message.contains("too long"));
    }

    #[test]
    fn governance_parser_enforces_every_record_category_limit_during_parsing() {
        fn assert_limit(record_kind: &str, limit: usize, mut record: impl FnMut(usize) -> String) {
            let mut source = String::from("schema 2\n");
            for index in 0..=limit {
                source.push_str(&record(index));
            }
            source.push_str(&"x".repeat(MAX_REGISTRY_LINE_BYTES + 1));
            source.push('\n');
            let error = parse_governance_state(source.as_bytes()).unwrap_err();
            assert_eq!(
                error.code.as_deref(),
                Some("invalid_registry_governance"),
                "{record_kind} records must use the bounded parser error"
            );
            assert!(
                error.message.contains(&format!(
                    "{record_kind} record limit {limit} exceeded at line {}",
                    limit + 2
                )),
                "unexpected {record_kind} limit diagnostic: {}",
                error.message
            );
            assert!(
                !error.message.contains("too long"),
                "{record_kind} overflow must stop before parsing trailing input"
            );
        }

        assert_limit("team", MAX_GOVERNANCE_RECORDS, |index| {
            format!("team t{index}\n")
        });
        assert_limit("member", MAX_GOVERNANCE_RECORDS, |index| {
            format!("member t{index} d{index}\n")
        });
        assert_limit("owner", MAX_GOVERNANCE_RECORDS, |index| {
            format!("owner p{index} developer:d\n")
        });
        assert_limit("token", MAX_GOVERNANCE_RECORDS, |index| {
            let mut token_hash = [0u8; 32];
            token_hash[..8].copy_from_slice(&(index as u64).to_be_bytes());
            format!("token sha256-{} d all\n", encode_hex(&token_hash))
        });
        let zero_hash = "0".repeat(64);
        assert_limit("audit", MAX_GOVERNANCE_AUDITS, |_| {
            format!("audit 1 root {zero_hash} {zero_hash} {zero_hash} init a\n")
        });
    }

    #[test]
    fn parser_enforces_utf8_line_entry_and_file_bounds() {
        assert!(parse_credential_records(&[0xff]).is_err());
        assert!(
            parse_credential_records(&vec![b'x'; MAX_CREDENTIAL_FILE_BYTES as usize + 1]).is_err()
        );
        let long = format!("#{}\n", "x".repeat(MAX_REGISTRY_LINE_BYTES));
        assert!(parse_credential_records(long.as_bytes()).is_err());
        let duplicate_hash: [u8; 32] = Sha256::digest(b"same").into();
        let line = format!("sha256-{} math\n", encode_hex(&duplicate_hash));
        assert!(parse_credential_records(format!("{line}{line}").as_bytes()).is_err());
        assert!(parse_credential_records(b"").unwrap().is_empty());
    }
}
