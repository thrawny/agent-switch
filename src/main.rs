mod daemon;
mod projects;
mod state;
#[cfg(feature = "niri")]
mod themes;
mod tmux;
mod track;

#[cfg(feature = "niri")]
mod niri;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agent-switch",
    about = "Track and switch between AI agent sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Handle hook events from agents (reads JSON from stdin)
    Track {
        /// Override the agent name carried in the hook payload
        #[arg(long)]
        agent: Option<String>,
        /// Event type: session-start, session-end, prompt-submit, stop, notification
        event: String,
    },
    /// Re-associate focused window with orphan session
    Fix,
    /// List all sessions as JSON
    List,
    /// Remove stale sessions
    Cleanup,
    /// Tmux picker (daemonless)
    Tmux {
        /// Skip keyboard UI, go straight to fzf search
        #[arg(long)]
        fzf: bool,
    },
    /// Run the daemon (session cache + file watchers)
    Serve {
        /// Enable niri GTK overlay (Linux only)
        #[cfg(feature = "niri")]
        #[arg(long)]
        niri: bool,
    },
    /// Niri GTK daemon (deprecated, use `serve --niri`)
    #[cfg(feature = "niri")]
    Niri {
        /// Toggle visibility (send to running daemon)
        #[arg(long)]
        toggle: bool,
        /// Toggle agents-only view (send to running daemon)
        #[arg(long)]
        toggle_agents: bool,
        /// Show demo overlay with mock data
        #[arg(long)]
        demo: bool,
        /// Override theme (e.g. "default", "molokai")
        #[arg(long)]
        theme: Option<String>,
    },
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    match cli.command {
        Command::Track { event, agent } => {
            if !track::handle_event(&event, agent.as_deref()) {
                std::process::exit(1);
            }
        }
        Command::Fix => todo!("fix command"),
        Command::List => {
            let store = match state::with_locked_store(|store| {
                state::cleanup_stale(store);
                Ok(store.clone())
            }) {
                Ok(store) => store,
                Err(err) => {
                    eprintln!("Failed to load state: {}", err);
                    std::process::exit(1);
                }
            };
            match serde_json::to_string_pretty(&store) {
                Ok(json) => println!("{}", json),
                Err(err) => {
                    eprintln!("Failed to serialize state for output: {}", err);
                    std::process::exit(1);
                }
            }
        }
        Command::Cleanup => {
            if let Err(err) = state::with_locked_store(|store| {
                state::cleanup_stale(store);
                Ok(())
            }) {
                eprintln!("Failed to update state: {}", err);
                std::process::exit(1);
            }
        }
        Command::Tmux { fzf } => {
            if fzf {
                tmux::run_fzf_only();
            } else {
                tmux::run();
            }
        }
        #[cfg(feature = "niri")]
        Command::Serve { niri } => {
            if niri {
                let exit_code = niri::run_with_daemon();
                std::process::exit(exit_code.into());
            } else {
                daemon::run_headless();
            }
        }
        #[cfg(not(feature = "niri"))]
        Command::Serve {} => {
            daemon::run_headless();
        }
        #[cfg(feature = "niri")]
        Command::Niri {
            toggle,
            toggle_agents,
            demo,
            theme,
        } => {
            let exit_code = if demo {
                niri::run_demo(theme.as_deref())
            } else if toggle_agents {
                niri::run_toggle_agents()
            } else {
                niri::run(toggle)
            };
            std::process::exit(exit_code.into());
        }
    }
}
