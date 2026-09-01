use std::io::{self, Write};

use ku::{
    error::KuResult,
    registry_admin::{
        add_team_member_from_env, claim_package_from_env, create_developer_from_env,
        create_team_from_env, initialize_governance_from_env, issue_developer_token_from_env,
        issue_token_from_env, migrate_governance_from_env, parse_registry_command,
        remove_team_member_from_env, revoke_developer_token_from_env,
        revoke_developer_token_hash_from_env, revoke_token_from_env, transfer_package_from_env,
        verify_governance_from_env, write_audit_verification_confirmation,
        write_developer_issued_token, write_governance_confirmation, write_issued_token,
        write_revocation_confirmation, RegistryCommand, REGISTRY_USAGE,
    },
    registry_server::{RegistryServer, RegistryServerConfig},
};

fn main() {
    if let Err(err) = run() {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "{err}").and_then(|()| stderr.flush());
        std::process::exit(1);
    }
}

fn run() -> KuResult<()> {
    match parse_registry_command(std::env::args_os().skip(1))? {
        RegistryCommand::Serve => {
            let server = RegistryServer::bind(RegistryServerConfig::from_env()?)?;
            let address = server.local_addr()?;
            let public_key = server.public_key();
            println!("ku-registry listening on https://{address}/v1/");
            println!("registry.public_key = \"{public_key}\"");
            server.serve()
        }
        RegistryCommand::Help => {
            println!("{REGISTRY_USAGE}");
            Ok(())
        }
        RegistryCommand::Issue { package_name } => {
            let token = issue_token_from_env(&package_name)?;
            write_issued_token(&token, &mut io::stdout().lock())
        }
        RegistryCommand::Revoke { package_name } => {
            revoke_token_from_env(&package_name)?;
            write_revocation_confirmation(&package_name, &mut io::stdout().lock())
        }
        RegistryCommand::GovernanceInit { developer } => {
            initialize_governance_from_env(&developer)?;
            write_governance_confirmation(
                &format!("initialized governance owner {developer}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::GovernanceMigrate { developer } => {
            migrate_governance_from_env(&developer)?;
            write_governance_confirmation(
                &format!("migrated governance owner {developer}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::DeveloperCreate { developer } => {
            create_developer_from_env(&developer)?;
            write_governance_confirmation(
                &format!("created developer {developer}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::DeveloperTokenIssue { developer } => {
            let token = issue_developer_token_from_env(&developer)?;
            write_developer_issued_token(&developer, &token, &mut io::stdout().lock())
        }
        RegistryCommand::DeveloperTokenRevoke { developer } => {
            revoke_developer_token_from_env(&developer)?;
            write_governance_confirmation(
                &format!("revoked developer token for {developer}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::DeveloperTokenRevokeHash {
            developer,
            token_hash,
        } => {
            revoke_developer_token_hash_from_env(&developer, token_hash)?;
            write_governance_confirmation(
                &format!("revoked developer token hash for {developer}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::TeamCreate { team } => {
            create_team_from_env(&team)?;
            write_governance_confirmation(&format!("created team {team}"), &mut io::stdout().lock())
        }
        RegistryCommand::TeamMemberAdd { team, developer } => {
            add_team_member_from_env(&team, &developer)?;
            write_governance_confirmation(
                &format!("added developer {developer} to team {team}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::TeamMemberRemove { team, developer } => {
            remove_team_member_from_env(&team, &developer)?;
            write_governance_confirmation(
                &format!("removed developer {developer} from team {team}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::PackageClaim {
            package_name,
            owner,
        } => {
            claim_package_from_env(&package_name, &owner)?;
            write_governance_confirmation(
                &format!("claimed package {package_name} for {owner}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::PackageTransfer {
            package_name,
            owner,
        } => {
            transfer_package_from_env(&package_name, &owner)?;
            write_governance_confirmation(
                &format!("transferred package {package_name} to {owner}"),
                &mut io::stdout().lock(),
            )
        }
        RegistryCommand::AuditVerify => {
            verify_governance_from_env()?;
            write_audit_verification_confirmation(&mut io::stdout().lock())
        }
    }
}
