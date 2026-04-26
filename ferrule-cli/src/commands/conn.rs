use super::{ConnArgs, ConnCommand};
use crate::error::CliError;
use ferrule_config::registry::ConnectionRegistry;

pub async fn run(args: ConnArgs) -> Result<(), CliError> {
    match args.command {
        ConnCommand::Add { name, url } => {
            let mut registry = ConnectionRegistry::load_default().map_err(CliError::registry)?;
            registry
                .add(name.clone(), url)
                .map_err(CliError::registry)?;
            registry.save_default().map_err(CliError::registry)?;
            println!("Connection '{}' added.", name);
        }
        ConnCommand::List => {
            let registry = ConnectionRegistry::load_default().map_err(CliError::registry)?;
            for entry in registry.list() {
                println!("{} => {}", entry.name, entry.url);
            }
        }
        ConnCommand::Remove { name } => {
            let mut registry = ConnectionRegistry::load_default().map_err(CliError::registry)?;
            registry
                .remove(&name)
                .map_err(CliError::registry)?;
            registry.save_default().map_err(CliError::registry)?;
            println!("Connection '{}' removed.", name);
        }
        ConnCommand::Test { name, conn_flags } => {
            let url = super::resolve_connection(&name, None).await?;
            let opts = ferrule_core::connection::ConnectOptions {
                insecure: conn_flags.insecure,
            };
            if opts.insecure {
                eprintln!("Warning: --insecure disables TLS certificate verification.");
            }
            let mut conn = ferrule_core::backend::connect(&url, &opts)
                .await
                .map_err(CliError::connection)?;
            conn.ping()
                .await
                .map_err(CliError::connection)?;
            println!("Connection '{}' is alive.", name);
        }
    }
    Ok(())
}
