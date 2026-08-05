mod config;
mod daemon;
mod niri;
mod sidebar_proto;
mod sidebar_proto_live;
mod state;
mod themes;
mod track;

use clap::{Parser, Subcommand};

fn focused_niri_window_id() -> Result<String, String> {
    let output = std::process::Command::new("niri")
        .args(["msg", "--json", "focused-window"])
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|err| err.to_string())?;
    value
        .get("id")
        .and_then(|id| id.as_u64())
        .map(|id| id.to_string())
        .ok_or_else(|| "focused window JSON did not contain numeric id".to_string())
}

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
        /// Override the session name carried in the hook payload
        #[arg(long)]
        session_name: Option<String>,
        /// Event type: session-start, session-end, prompt-submit, stop, notification
        event: String,
    },
    /// List all sessions as JSON
    List,
    /// Print the session for the focused niri window as JSON
    Focused,
    /// Remove stale sessions
    Cleanup,
    /// Run the daemon (session cache + file watchers)
    Serve {
        /// Enable the GTK agents overlay
        #[arg(long)]
        niri: bool,
    },
    /// Toggle the agents overlay (sends to the running daemon)
    Toggle,
    /// Show a demo overlay with mock data
    Demo {
        /// Override theme (e.g. "default", "molokai")
        #[arg(long)]
        theme: Option<String>,
    },
    /// PROTOTYPE: ticket-06 sidebar (throwaway); --live joins real sessions
    DemoSidebar {
        /// Show real agent sessions and act on real windows (ticket 08)
        #[arg(long)]
        live: bool,
    },
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    match cli.command {
        Command::Track {
            event,
            agent,
            session_name,
        } => {
            if !track::handle_event(&event, agent.as_deref(), session_name.as_deref()) {
                std::process::exit(1);
            }
        }
        Command::List => {
            let store = match state::with_locked_store(|store| {
                state::cleanup_stale(store);
                daemon::refresh_transcript_derived_states(store);
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
        Command::Focused => {
            let focused_id = match focused_niri_window_id() {
                Ok(id) => id,
                Err(err) => {
                    eprintln!("Failed to get focused niri window: {err}");
                    std::process::exit(1);
                }
            };
            let session = match state::with_locked_store(|store| {
                state::cleanup_stale(store);
                daemon::refresh_transcript_derived_states(store);
                Ok(store.sessions.get(&focused_id).cloned())
            }) {
                Ok(session) => session,
                Err(err) => {
                    eprintln!("Failed to load state: {}", err);
                    std::process::exit(1);
                }
            };
            match session {
                Some(session) => println!("{}", serde_json::to_string_pretty(&session).unwrap()),
                None => std::process::exit(1),
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
        Command::Serve { niri } => {
            if niri {
                let exit_code = niri::run_with_daemon();
                std::process::exit(exit_code.into());
            } else {
                daemon::run_headless();
            }
        }
        Command::Toggle => {
            let exit_code = niri::run_toggle();
            std::process::exit(exit_code.into());
        }
        Command::Demo { theme } => {
            let exit_code = niri::run_demo(theme.as_deref());
            std::process::exit(exit_code.into());
        }
        Command::DemoSidebar { live } => {
            let exit_code = sidebar_proto::run(live);
            std::process::exit(exit_code.into());
        }
    }
}
