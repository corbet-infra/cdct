//! `cdct` — the conductor's command line.
//!
//! `serve` runs the daemon; `status`, `switch` and `watch` are its clients
//! (over the local socket, or a device path when run inside a guest); `plan
//! switch` is the offline planner that changes nothing. Exit status: 0 plan or
//! already held, 2 wait, 3 decline, 1 error.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cdct::client::{Client, Endpoint};
use cdct::config::Config;
use cdct::protocol::{Reply, Request};
use cdct::{
    Conductor, FactsFile, GpuState, MemorySnapshot, Policy, ProcReader, SwitchOutcome, describe,
    holders::read_table,
};
use clap::{Parser, Subcommand};
use serde::Serialize;

/// The conductor: decides who gets RAM and the GPU, and never executes.
#[derive(Debug, Parser)]
#[command(name = "cdct", version, about)]
struct Cli {
    /// The daemon endpoint: a Unix socket path, or `device:<path>`.
    #[arg(long, global = true)]
    endpoint: Option<String>,
    /// The configuration file; defaults to $CDCT_CONFIG, then the built-in
    /// defaults.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// The holder table for `serve`; defaults to $CDCT_STATE or
    /// /var/lib/cdct/holders.json.
    #[arg(long, global = true)]
    state: Option<PathBuf>,
    /// A policy JSON file; missing fields keep their defaults.
    #[arg(long, global = true)]
    policy: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon: the one writer, its listeners and its pushed events.
    Serve {
        /// The local control socket; overrides the configuration.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Ask the daemon who holds the card and what each target would do.
    Status {
        /// Print the status as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Switch the card to a configured kernel (`macos`, `windows`, `linux`).
    Switch {
        /// A configured target name, or `host` / `guest:<instance>`.
        name: String,
        /// Print the outcome as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the daemon's event stream until interrupted.
    Watch {
        /// Print every message as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a decision; change nothing.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PlanCommand {
    /// Plan a switch of the card to a kernel, offline.
    Switch {
        /// A configured target name (`macos`, `windows`, `linux`), `host` or
        /// `guest:<instance>`.
        kernel: String,
        /// A JSON facts document; without it, live `/proc` facts are read and
        /// the card's owner comes from the holder table.
        #[arg(long)]
        facts: Option<PathBuf>,
        /// Print the outcome as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        #[cfg(unix)]
        Command::Serve { socket } => serve(&cli, socket.as_deref()),
        #[cfg(not(unix))]
        Command::Serve { .. } => fail("cdct serve needs a Unix host"),
        Command::Status { json } => status(&cli, *json),
        Command::Switch { name, json } => switch(&cli, name, *json),
        Command::Watch { json } => watch(&cli, *json),
        Command::Plan {
            command:
                PlanCommand::Switch {
                    kernel,
                    facts,
                    json,
                },
        } => plan_switch(&cli, kernel, facts.as_deref(), *json),
    }
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("cdct: {message}");
    ExitCode::FAILURE
}

fn load_config(explicit: Option<&Path>) -> Result<Config, String> {
    match explicit {
        Some(path) => Config::load(path).map_err(|error| error.to_string()),
        None => match Config::default_path() {
            Some(path) if path.exists() => Config::load(&path).map_err(|error| error.to_string()),
            _ => Ok(Config::default()),
        },
    }
}

fn client_endpoint(cli: &Cli, config: &Config) -> Endpoint {
    cli.endpoint.as_deref().map_or_else(
        || Endpoint::LocalSocket(config.socket.clone()),
        Endpoint::parse,
    )
}

#[cfg(unix)]
fn serve(cli: &Cli, socket: Option<&Path>) -> ExitCode {
    let mut config = match load_config(cli.config.as_deref()) {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    if let Some(policy) = &cli.policy {
        match Policy::load(Some(policy)) {
            Ok(loaded) => config.policy = loaded,
            Err(error) => return fail(error),
        }
    }
    if let Some(socket) = socket {
        config.socket = socket.to_path_buf();
    }
    if let Some(state) = cli
        .state
        .clone()
        .or_else(|| std::env::var_os("CDCT_STATE").map(PathBuf::from))
    {
        config.state = state;
    }
    let running = match cdct::daemon::start(config) {
        Ok(running) => running,
        Err(error) => return fail(error),
    };
    println!("cdct: listening on {}", running.local_socket().display());
    for (instance, path) in running.guest_sockets() {
        println!("cdct: guest {instance} on {}", path.display());
    }
    let _ = std::io::stdout().flush();
    loop {
        std::thread::park();
    }
}

fn status(cli: &Cli, json: bool) -> ExitCode {
    let config = match load_config(cli.config.as_deref()) {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let endpoint = client_endpoint(cli, &config);
    let mut client = match Client::connect(&endpoint) {
        Ok(client) => client,
        Err(error) => return fail(error),
    };
    match client.ask(&Request::Status) {
        Ok(Reply::Status { status }) => {
            if json {
                print_json(&status);
            } else {
                for line in describe::status(&status) {
                    println!("{line}");
                }
            }
            ExitCode::SUCCESS
        }
        Ok(other) => fail(format!("unexpected reply: {other:?}")),
        Err(error) => fail(error),
    }
}

fn switch(cli: &Cli, name: &str, json: bool) -> ExitCode {
    let config = match load_config(cli.config.as_deref()) {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let Some(target) = config.resolve(name) else {
        let names = config.target_names();
        return fail(format!(
            "unknown kernel {name:?}; configured kernels: {}",
            if names.is_empty() {
                "(none)".to_owned()
            } else {
                names.join(", ")
            }
        ));
    };
    let endpoint = client_endpoint(cli, &config);
    let mut client = match Client::connect(&endpoint) {
        Ok(client) => client,
        Err(error) => return fail(error),
    };
    match client.ask(&Request::Switch { target }) {
        Ok(Reply::Switch { outcome }) => {
            if json {
                print_json(&outcome);
            } else {
                for line in describe::outcome(&outcome) {
                    println!("{line}");
                }
            }
            outcome_exit(&outcome)
        }
        Ok(other) => fail(format!("unexpected reply: {other:?}")),
        Err(error) => fail(error),
    }
}

fn watch(cli: &Cli, json: bool) -> ExitCode {
    let config = match load_config(cli.config.as_deref()) {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let endpoint = client_endpoint(cli, &config);
    let mut client = match Client::connect(&endpoint) {
        Ok(client) => client,
        Err(error) => return fail(error),
    };
    if let Err(error) = client.send(&Request::Subscribe) {
        return fail(error);
    }
    let mut stdout = std::io::stdout();
    loop {
        let reply = match client.receive() {
            Ok(Some(reply)) => reply,
            Ok(None) => return ExitCode::SUCCESS,
            Err(error) => return fail(error),
        };
        if json {
            let _ = writeln!(
                stdout,
                "{}",
                serde_json::to_string(&reply).unwrap_or_default()
            );
        } else {
            match &reply {
                Reply::Subscribed { channel, status } => {
                    let _ = writeln!(stdout, "subscribed on {}", describe::channel(channel));
                    for line in describe::status(status) {
                        let _ = writeln!(stdout, "{line}");
                    }
                }
                Reply::Status { status } => {
                    for line in describe::status(status) {
                        let _ = writeln!(stdout, "{line}");
                    }
                }
                Reply::Event { event } => {
                    let _ = writeln!(stdout, "{}", describe::event(event));
                }
                other => {
                    let _ = writeln!(stdout, "{other:?}");
                }
            }
        }
        let _ = stdout.flush();
    }
}

fn plan_switch(cli: &Cli, kernel: &str, facts: Option<&Path>, json: bool) -> ExitCode {
    let config = match load_config(cli.config.as_deref()) {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    let Some(kernel) = config.resolve(kernel) else {
        return fail(format!("unknown kernel `{kernel}`"));
    };
    let kernel = &kernel;
    let policy = match &cli.policy {
        Some(path) => match Policy::load(Some(path)) {
            Ok(policy) => policy,
            Err(error) => return fail(error),
        },
        None => config.policy,
    };
    let state = cli
        .state
        .clone()
        .or_else(|| std::env::var_os("CDCT_STATE").map(PathBuf::from))
        .unwrap_or(config.state);
    let conductor = Conductor::new(policy);
    let table = match read_table(&state) {
        Ok(table) => table.unwrap_or_default(),
        Err(error) => return fail(error),
    };
    let (memory, gpu) = match facts {
        Some(path) => match FactsFile::read(path) {
            Ok(facts) => match facts.memory_snapshot(path) {
                Ok(memory) => (memory, facts.gpu),
                Err(error) => return fail(error),
            },
            Err(error) => return fail(error),
        },
        None => match live_facts(&table) {
            Ok(facts) => facts,
            Err(error) => return fail(error),
        },
    };
    let outcome = conductor.switch(kernel, &gpu, &memory, &table);
    if json {
        print_json(&outcome);
    } else {
        for line in describe::outcome(&outcome) {
            println!("{line}");
        }
    }
    outcome_exit(&outcome)
}

fn live_facts(table: &cdct::HolderTable) -> Result<(MemorySnapshot, GpuState), String> {
    let raw = ProcReader::system()
        .raw_facts()
        .map_err(|error| format!("cannot read /proc: {error}"))?;
    let memory = MemorySnapshot::parse(&raw).map_err(|error| error.to_string())?;
    let gpu = GpuState {
        owner: table.gpu_owner.clone(),
        ..GpuState::default()
    };
    Ok((memory, gpu))
}

fn outcome_exit(outcome: &SwitchOutcome) -> ExitCode {
    match outcome {
        SwitchOutcome::Plan(_) | SwitchOutcome::AlreadyHeld { .. } => ExitCode::SUCCESS,
        SwitchOutcome::Wait { .. } => ExitCode::from(2),
        SwitchOutcome::Decline { .. } => ExitCode::from(3),
    }
}

fn print_json(value: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("the value serialises")
    );
}
