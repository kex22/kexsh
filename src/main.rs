use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "kexsh", about = "A modern terminal multiplexer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the kexsh server
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Manage terminal instances
    Terminal {
        #[command(subcommand)]
        action: TerminalAction,
    },
    /// Manage views
    View {
        #[command(subcommand)]
        action: ViewAction,
    },
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Login to kexsh cloud
    Login {
        /// Server URL
        #[arg(long, default_value = "https://app.kex.sh")]
        server: String,
    },
    /// Logout from kexsh cloud
    Logout,
    /// Manage URL proxies for local ports
    Proxy {
        #[command(subcommand)]
        action: ProxyAction,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the server daemon
    Start,
    /// Stop the server daemon
    Stop,
}

#[derive(Subcommand)]
enum TerminalAction {
    /// Create a new terminal
    Create {
        /// Optional name for the terminal
        #[arg(long)]
        name: Option<String>,
        /// Don't attach after creating
        #[arg(long)]
        detach: bool,
    },
    /// List all terminals
    Ls,
    /// Kill a terminal
    Kill {
        /// Terminal ID or name
        id: String,
    },
    /// Attach to a terminal
    Attach {
        /// Terminal ID or name
        id: String,
    },
    /// Sync a terminal to the cloud
    Sync {
        /// Terminal ID or name
        id: String,
    },
    /// Unsync a terminal from the cloud
    Unsync {
        /// Terminal ID or name
        id: String,
    },
}

#[derive(Subcommand)]
enum ViewAction {
    /// Create a new view
    Create {
        /// View name
        #[arg(long)]
        name: Option<String>,
        /// Initial terminal ID
        terminal: String,
    },
    /// List all views
    Ls,
    /// Delete a view
    Rm {
        /// View ID or name
        id: String,
    },
    /// Show view details
    Show {
        /// View ID or name
        id: String,
    },
    /// Attach to a view (restore layout)
    Attach {
        /// View ID or name
        id: String,
    },
    /// Add a terminal to a view
    Add {
        /// View ID or name
        view: String,
        /// Terminal ID
        terminal: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    Show,
    /// Show configuration file path
    Path,
}

#[derive(Subcommand)]
enum ProxyAction {
    /// Expose a local port via URL
    Create {
        /// Port number to expose
        port: u16,
        /// Make publicly accessible (default: private, owner-only)
        #[arg(long)]
        public: bool,
    },
    /// Stop exposing a port
    Rm {
        /// Port number to unexpose
        port: u16,
    },
    /// List exposed ports
    Ls,
}

fn main() {
    let cli = Cli::parse();

    // Daemonize before tokio runtime is created
    if matches!(
        cli.command,
        Command::Server {
            action: ServerAction::Start
        }
    ) && let Err(e) = kexsh::server::daemon::daemonize()
    {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    if let Err(e) = rt.block_on(run(cli)) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> kexsh::error::Result<()> {
    use kexsh::error::KexshError;
    use kexsh::ipc::client::IpcClient;
    use kexsh::ipc::message::{Request, Response};

    match cli.command {
        Command::Server { action } => match action {
            ServerAction::Start => kexsh::server::Server::start().await,
            ServerAction::Stop => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::ServerStop).await? {
                    Response::Ok => {
                        println!("server stopped");
                        Ok(())
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
        },
        Command::Terminal { action } => match action {
            TerminalAction::Create { name, detach } => {
                let label = name.clone();
                let mut client = IpcClient::connect().await?;
                match client.send(Request::TerminalCreate { name }).await? {
                    Response::TerminalCreated { id } => {
                        if detach {
                            println!("{id}");
                            Ok(())
                        } else {
                            let label = label.unwrap_or_else(|| id.clone());
                            kexsh::terminal::attach::attach(&label).await
                        }
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            TerminalAction::Ls => {
                let mut client = IpcClient::connect().await?;
                if let Response::TerminalList { terminals } =
                    client.send(Request::TerminalList).await?
                {
                    if terminals.is_empty() {
                        println!("no terminals");
                    } else {
                        println!("{:<10} {:<15} CREATED", "ID", "NAME");
                        for t in terminals {
                            println!(
                                "{:<10} {:<15} {}",
                                t.id,
                                t.name.as_deref().unwrap_or("-"),
                                t.created_at
                            );
                        }
                    }
                }
                Ok(())
            }
            TerminalAction::Kill { id } => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::TerminalKill { id }).await? {
                    Response::Ok => Ok(()),
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            TerminalAction::Attach { id } => kexsh::terminal::attach::attach(&id).await,
            TerminalAction::Sync { id } => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::TerminalSync { id }).await? {
                    Response::SyncStatus { synced: true } => {
                        println!("terminal synced");
                        Ok(())
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            TerminalAction::Unsync { id } => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::TerminalUnsync { id }).await? {
                    Response::SyncStatus { synced: false } => {
                        println!("terminal unsynced");
                        Ok(())
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
        },
        Command::View { action } => match action {
            ViewAction::Create { name, terminal } => {
                let mut client = IpcClient::connect().await?;
                match client
                    .send(Request::ViewCreate {
                        name,
                        terminal_id: terminal,
                    })
                    .await?
                {
                    Response::ViewCreated { id } => {
                        println!("{id}");
                        Ok(())
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            ViewAction::Ls => {
                let mut client = IpcClient::connect().await?;
                if let Response::ViewList { views } = client.send(Request::ViewList).await? {
                    if views.is_empty() {
                        println!("no views");
                    } else {
                        println!("{:<10} {:<15} TERMINALS", "ID", "NAME");
                        for v in views {
                            println!(
                                "{:<10} {:<15} {}",
                                v.id,
                                v.name.as_deref().unwrap_or("-"),
                                v.terminal_ids.join(", ")
                            );
                        }
                    }
                }
                Ok(())
            }
            ViewAction::Rm { id } => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::ViewDelete { id }).await? {
                    Response::Ok => Ok(()),
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            ViewAction::Show { id } => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::ViewShow { id }).await? {
                    Response::ViewShow { view } => {
                        println!("ID:        {}", view.id);
                        println!("Name:      {}", view.name.as_deref().unwrap_or("-"));
                        println!("Terminals: {}", view.terminal_ids.join(", "));
                        println!("Created:   {}", view.created_at);
                        Ok(())
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            ViewAction::Attach { id } => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::ViewAttach { id: id.clone() }).await? {
                    Response::ViewAttach {
                        terminal_ids,
                        layout,
                        focused,
                    } => {
                        if terminal_ids.is_empty() {
                            return Err(KexshError::Server("view has no terminals".into()));
                        }
                        let label = terminal_ids[0].clone();
                        kexsh::terminal::attach::attach_view(
                            &label,
                            &terminal_ids[1..],
                            Some(&id),
                            layout,
                            focused,
                        )
                        .await
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            ViewAction::Add { view, terminal } => {
                let mut client = IpcClient::connect().await?;
                match client
                    .send(Request::ViewAddTerminal {
                        view_id: view,
                        terminal_id: terminal,
                    })
                    .await?
                {
                    Response::Ok => Ok(()),
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
        },
        Command::Config { action } => match action {
            ConfigAction::Show => {
                let cfg = kexsh::config::Config::load().unwrap_or_default();
                println!("prefix = \"{}\"", cfg.prefix.to_config_string());
                println!("status_bar = {}", cfg.status_bar);
                Ok(())
            }
            ConfigAction::Path => {
                println!("{}", kexsh::config::config_path().display());
                Ok(())
            }
        },
        Command::Login { server } => kexsh::cloud::login::login(&server).await,
        Command::Logout => kexsh::cloud::login::logout().await,
        Command::Proxy { action } => match action {
            ProxyAction::Create { port, public } => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::ProxyExpose { port, public }).await? {
                    Response::ProxyExposed { port, url } => {
                        println!("port {port} exposed: {url}");
                        Ok(())
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            ProxyAction::Rm { port } => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::ProxyUnexpose { port }).await? {
                    Response::Ok => {
                        println!("port {port} unexposed");
                        Ok(())
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
            ProxyAction::Ls => {
                let mut client = IpcClient::connect().await?;
                match client.send(Request::ProxyList).await? {
                    Response::ProxyList { ports } => {
                        if ports.is_empty() {
                            println!("no exposed ports");
                        } else {
                            println!("{:<8} {:<8} URL", "PORT", "ACCESS");
                            for p in ports {
                                let access = if p.public { "public" } else { "private" };
                                println!(
                                    "{:<8} {:<8} {}",
                                    p.port,
                                    access,
                                    p.url.as_deref().unwrap_or("-")
                                );
                            }
                        }
                        Ok(())
                    }
                    Response::Error { message } => Err(KexshError::Server(message)),
                    _ => Ok(()),
                }
            }
        },
    }
}
