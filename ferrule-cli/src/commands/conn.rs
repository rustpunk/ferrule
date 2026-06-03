use super::{ConnArgs, ConnCommand};
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_config::registry::ConnectionRegistry;

pub fn run(args: ConnArgs, _global_config: &GlobalConfig) -> Result<(), CliError> {
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
            let resolved = super::resolve_connection(
                &name,
                None,
                conn_flags.ssh_tunnel.as_deref(),
                conn_flags.ssh_key.as_deref(),
                conn_flags.proxy_url.as_deref(),
                _global_config,
            )?;
            super::check_daemon_ssh_compat(conn_flags.daemon, &resolved)?;
            let opts = ferrule_sql::connection::ConnectOptions {
                insecure: conn_flags.insecure,
                password: None,
            };
            if opts.insecure {
                eprintln!("Warning: --insecure disables TLS certificate verification.");
            }
            let mut conn = super::connect_resolved(resolved, &opts)?;
            conn.ping().map_err(CliError::connection)?;
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
            let password = rpassword::prompt_password(prompt).map_err(CliError::Io)?;
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
            crate::daemon::start_daemon(background)?;
        }
        ConnCommand::Stop => {
            crate::daemon::stop_daemon()?;
        }
        ConnCommand::Status => {
            crate::daemon::daemon_status()?;
        }
        ConnCommand::Restart => {
            crate::daemon::stop_daemon().ok();
            crate::daemon::start_daemon(false)?;
        }
    }
    Ok(())
}
