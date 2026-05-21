use anyhow::Result;
use clap::{Parser, Subcommand};
use codex_gateway_core::config::default_data_dir;
use codex_gateway_core::{serve, test_gateway, GatewayOptions, Store};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "cockpit-codex")]
#[command(about = "Headless Codex Local API Gateway for AWS/Linux servers")]
struct Cli {
    #[arg(long, env = "COCKPIT_CODEX_HOME", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Import a Codex auth.json file into the gateway account store.
    ImportAuth {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        email: Option<String>,
    },
    /// Manage imported accounts.
    Accounts {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Manage the local gateway API key.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Show data directory, gateway key state, config, and imported accounts.
    Status,
    /// Start the OpenAI-compatible HTTP gateway.
    Serve {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        upstream_base_url: Option<String>,
    },
    /// Probe a running gateway with a /v1/responses request.
    Test {
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        base_url: String,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long, default_value = "gpt-5-codex")]
        model: String,
    },
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// List imported accounts.
    List,
    /// Remove an account by id, email, or name.
    Remove { account: String },
}

#[derive(Debug, Subcommand)]
enum KeyCommand {
    /// Print the gateway API key.
    Show,
    /// Rotate the gateway API key.
    Rotate,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let data_dir = match cli.data_dir {
        Some(path) => path,
        None => default_data_dir()?,
    };
    let store = Store::new(data_dir);

    match cli.command {
        Commands::ImportAuth { file, name, email } => {
            let imported = store.import_auth(&file, &name, email.as_deref())?;
            println!("Imported account:");
            println!("  id: {}", imported.account.id);
            println!("  name: {}", imported.account.name);
            println!("  email: {}", imported.account.email);
            println!("  source: {}", imported.source);
            println!("  refresh token: {}", imported.account.refresh_token.is_some());
        }
        Commands::Accounts { command } => match command {
            AccountCommand::List => {
                let accounts = store.list_account_summaries()?;
                if accounts.is_empty() {
                    println!("No accounts imported.");
                } else {
                    for account in accounts {
                        println!(
                            "{}\t{}\t{}\trefresh={}\texpired={}",
                            account.id,
                            account.name,
                            account.email,
                            account.has_refresh_token,
                            account.token_expired
                        );
                    }
                }
            }
            AccountCommand::Remove { account } => {
                let removed = store.remove_account(&account)?;
                println!("Removed {} ({})", removed.email, removed.id);
            }
        },
        Commands::Key { command } => match command {
            KeyCommand::Show => {
                let key = store.gateway_key()?;
                println!("{}", key.0);
            }
            KeyCommand::Rotate => {
                let key = store.rotate_gateway_key()?;
                println!("{}", key.0);
            }
        },
        Commands::Status => {
            store.init()?;
            let config = store.config()?;
            let key = store.gateway_key()?;
            let accounts = store.list_account_summaries()?;
            println!("data_dir: {}", store.data_dir().display());
            println!("listen: {}:{}", config.host, config.port);
            println!("upstream: {}", config.upstream_base_url);
            println!("gateway_key: {}...", &key.0[..key.0.len().min(8)]);
            println!("accounts: {}", accounts.len());
            for account in accounts {
                println!("  - {} {} ({})", account.id, account.email, account.name);
            }
        }
        Commands::Serve {
            host,
            port,
            upstream_base_url,
        } => {
            let config = store.config()?;
            let options = GatewayOptions {
                host: host.unwrap_or(config.host),
                port: port.unwrap_or(config.port),
                upstream_base_url: upstream_base_url.unwrap_or(config.upstream_base_url),
            };
            if options.host == "0.0.0.0" {
                eprintln!("WARNING: serving public plain HTTP. Use AWS Security Groups/IP allowlists; Bearer keys are not encrypted without TLS.");
            }
            let key = store.gateway_key()?;
            eprintln!("Gateway API key: {}", key.0);
            serve(store, options).await?;
        }
        Commands::Test {
            base_url,
            api_key,
            model,
        } => {
            let api_key = match api_key {
                Some(key) => key,
                None => store.gateway_key()?.0,
            };
            let result = test_gateway(&base_url, &api_key, &model).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }

    Ok(())
}

