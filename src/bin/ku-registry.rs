use std::io;

use ku::{
    error::KuResult,
    registry_admin::{
        issue_token_from_env, parse_registry_command, revoke_token_from_env, write_issued_token,
        write_revocation_confirmation, RegistryCommand, REGISTRY_USAGE,
    },
    registry_server::{RegistryServer, RegistryServerConfig},
};

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
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
    }
}
