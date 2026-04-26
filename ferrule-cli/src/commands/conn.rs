use super::{ConnArgs, ConnCommand};
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_config::registry::ConnectionRegistry;

pub async fn run(args: ConnArgs, _global_config: &GlobalConfig) -> Result<(), CliError> {
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
            registry.remove(&name).map_err(CliError::registry)?;
            registry.save_default().map_err(CliError::registry)?;
            println!("Connection '{}' removed.", name);
        }
        ConnCommand::Test { name, conn_flags } => {
            let url = super::resolve_connection(&name, None, _global_config).await?;
            let opts = ferrule_core::connection::ConnectOptions {
                insecure: conn_flags.insecure,
            };
            if opts.insecure {
                eprintln!("Warning: --insecure disables TLS certificate verification.");
            }
            let mut conn = ferrule_core::backend::connect(&url, &opts)
                .await
                .map_err(CliError::connection)?;
            conn.ping().await.map_err(CliError::connection)?;
            println!("Connection '{}' is alive.", name);
        }
        ConnCommand::SetPassword { name } => {
            let tty = is_terminal::IsTerminal::is_terminal(&std::io::stdin());
            if !tty {
                return Err(CliError::usage(
                    "Interactive terminal required to set password.",
                ));
            }
            let prompt = format!("Password for '{}': ", name);
            let password = tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt))
                .await
                .map_err(|e| CliError::usage(format!("Password prompt failed: {e}")))?
                .map_err(CliError::Io)?;
            if password.is_empty() {
                return Err(CliError::usage("Password cannot be empty."));
            }
            let secret = secrecy::SecretString::new(password.into());
            let store = hasp::Store::with_defaults();
            let url = format!("keyring://ferrule/{}", name);
            store.put(&url, &secret).map_err(|e| {
                CliError::registry(ferrule_config::error::ConfigError::HaspError(e.to_string()))
            })?;
            println!("Password stored in keyring for '{}'.", name);
        }
        ConnCommand::DeletePassword { name } => {
            let store = hasp::Store::with_defaults();
            let url = format!("keyring://ferrule/{}", name);
            store.delete(&url).map_err(|e| {
                CliError::registry(ferrule_config::error::ConfigError::HaspError(e.to_string()))
            })?;
            println!("Password removed from keyring for '{}'.", name);
        }
        ConnCommand::Start { background } => {
            crate::daemon::start_daemon(background).await?;
        }
        ConnCommand::Stop => {
            crate::daemon::stop_daemon().await?;
        }
        ConnCommand::Status => {
            crate::daemon::daemon_status().await?;
        }
        ConnCommand::Restart => {
            crate::daemon::stop_daemon().await.ok();
            crate::daemon::start_daemon(false).await?;
        }
    }
    Ok(())
}
