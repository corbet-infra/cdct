//! `cdct` — the conductor's command line.
//!
//! Two commands, no daemon: `status` shows the persisted holder table, and
//! `plan switch` prints the plan for a kernel change, or the reason it waits or
//! is declined. Exit status: 0 plan or already held, 2 wait, 3 decline, 1 error.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cdct::{
    Conductor, FactsFile, GpuState, KernelOwner, MemoryRefusal, MemorySnapshot, PlanStep, Policy,
    ProcReader, SwitchOutcome, holders::read_table,
};
use cdrv::InstanceId;
use clap::{Parser, Subcommand};
use serde_json::json;

/// The conductor: decides who gets RAM and the GPU, and never executes.
#[derive(Debug, Parser)]
#[command(name = "cdct", version, about)]
struct Cli {
    /// The persisted holder table; defaults to $CDCT_STATE or /var/lib/cdct/holders.json.
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
    /// Show the persisted holder table.
    Status {
        /// Print the table as JSON.
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
    /// Plan a switch of the card to a kernel.
    Switch {
        /// `host` or `guest:<instance>`.
        #[arg(value_parser = parse_kernel)]
        kernel: KernelOwner,
        /// A JSON facts document; without it, live `/proc` facts are read and
        /// the card's owner comes from the holder table.
        #[arg(long)]
        facts: Option<PathBuf>,
        /// Print the outcome as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn parse_kernel(value: &str) -> Result<KernelOwner, String> {
    if value == "host" {
        return Ok(KernelOwner::Host);
    }
    match value.strip_prefix("guest:") {
        Some(id) if !id.is_empty() => Ok(KernelOwner::Guest(InstanceId::from(id))),
        _ => Err(format!(
            "kernel must be `host` or `guest:<instance>`, got {value:?}"
        )),
    }
}

fn state_path(cli: &Cli) -> PathBuf {
    cli.state
        .clone()
        .or_else(|| std::env::var_os("CDCT_STATE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/var/lib/cdct/holders.json"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let state = state_path(&cli);
    match &cli.command {
        Command::Status { json } => status(&state, *json),
        Command::Plan {
            command:
                PlanCommand::Switch {
                    kernel,
                    facts,
                    json,
                },
        } => plan_switch(
            cli.policy.as_deref(),
            &state,
            kernel,
            facts.as_deref(),
            *json,
        ),
    }
}

fn status(state: &Path, json: bool) -> ExitCode {
    match read_table(state) {
        Ok(table) => {
            let table = table.unwrap_or_default();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "path": state.display().to_string(),
                        "present": state.exists(),
                        "table": table,
                    }))
                    .expect("the holder table serialises")
                );
            } else if !state.exists() {
                println!("no holder table at {}", state.display());
            } else {
                println!("gpu owner: {}", table.gpu_owner);
                println!("sequence: {}", table.seq);
                println!("holders: {}", table.holders.len());
                for holder in &table.holders {
                    let instance = holder
                        .instance
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), InstanceId::to_string);
                    let tenant = holder.tenant.as_deref().unwrap_or("-");
                    println!(
                        "  {} instance={} tenant={} class={:?} priority={} memory={}MiB devices={}",
                        holder.id,
                        instance,
                        tenant,
                        holder.class,
                        holder.priority,
                        holder.memory_mib,
                        if holder.devices.is_empty() {
                            "-".to_owned()
                        } else {
                            holder.devices.join(",")
                        }
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cdct: {error}");
            ExitCode::FAILURE
        }
    }
}

fn plan_switch(
    policy_path: Option<&Path>,
    state: &Path,
    kernel: &KernelOwner,
    facts_path: Option<&Path>,
    json: bool,
) -> ExitCode {
    let policy = match Policy::load(policy_path) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("cdct: {error}");
            return ExitCode::FAILURE;
        }
    };
    let conductor = Conductor::new(policy);
    let (memory, gpu) = match facts_path {
        Some(path) => match FactsFile::read(path) {
            Ok(facts) => match facts.memory_snapshot(path) {
                Ok(memory) => (memory, facts.gpu),
                Err(error) => {
                    eprintln!("cdct: {error}");
                    return ExitCode::FAILURE;
                }
            },
            Err(error) => {
                eprintln!("cdct: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => match live_facts(state) {
            Ok(facts) => facts,
            Err(error) => {
                eprintln!("cdct: {error}");
                return ExitCode::FAILURE;
            }
        },
    };
    let outcome = conductor.switch(kernel, &gpu, &memory);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome).expect("the switch outcome serialises")
        );
    } else {
        print_outcome(&outcome);
    }
    match outcome {
        SwitchOutcome::Plan(_) | SwitchOutcome::AlreadyHeld { .. } => ExitCode::SUCCESS,
        SwitchOutcome::Wait { .. } => ExitCode::from(2),
        SwitchOutcome::Decline { .. } => ExitCode::from(3),
    }
}

fn live_facts(state: &Path) -> Result<(MemorySnapshot, GpuState), String> {
    let raw = ProcReader::system()
        .raw_facts()
        .map_err(|error| format!("cannot read /proc: {error}"))?;
    let memory = MemorySnapshot::parse(&raw).map_err(|error| error.to_string())?;
    let table = read_table(state)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let gpu = GpuState {
        owner: table.gpu_owner,
        ..GpuState::default()
    };
    Ok((memory, gpu))
}

fn print_outcome(outcome: &SwitchOutcome) {
    match outcome {
        SwitchOutcome::AlreadyHeld { kernel } => {
            println!("the card already belongs to {kernel}");
        }
        SwitchOutcome::Plan(plan) => {
            println!("plan: switch the card to {}", plan.target);
            if let Some(memory) = &plan.memory {
                println!(
                    "  memory: {} MiB admitted ({} MiB available, {} MiB freed by the plan, \
                     {} MiB from swap)",
                    memory.required_mib,
                    memory.available_mib,
                    memory.credited_mib,
                    memory.via_swap_mib
                );
            }
            for (index, step) in plan.steps.iter().enumerate() {
                println!("  {}. {}", index + 1, step_line(step));
            }
        }
        SwitchOutcome::Wait { reason } => {
            println!("wait: the target is not admitted now");
            println!("  {}", refusal_line(reason));
        }
        SwitchOutcome::Decline { reason } => {
            println!("decline: the target cannot be admitted");
            println!("  {reason:?}");
        }
    }
}

fn step_line(step: &PlanStep) -> String {
    match step {
        PlanStep::StopHostTenant { tenant, ladder } => {
            format!("stop host tenant {tenant} ({ladder})")
        }
        PlanStep::StopInstance { instance, mode } => {
            format!("stop instance {instance} ({mode:?})")
        }
        PlanStep::HandOverCard { from, to } => format!("hand over the card: {from} -> {to}"),
        PlanStep::StartInstance { instance } => format!("start instance {instance}"),
        PlanStep::StartHostTenant { tenant, ladder } => {
            format!("start host tenant {tenant} ({ladder})")
        }
        PlanStep::SunshineFollows { kernel } => format!("Sunshine follows {kernel}"),
    }
}

fn refusal_line(reason: &MemoryRefusal) -> String {
    match reason {
        MemoryRefusal::NotEnoughFree {
            available_mib,
            required_mib,
            gap_mib,
            ..
        } => format!(
            "not enough free memory: {required_mib} MiB needed, {available_mib} MiB available \
             ({gap_mib} MiB short); nothing is displaced"
        ),
        MemoryRefusal::SwapNotAllowedUnderPressure {
            gap_mib,
            some_avg10,
            full_avg10,
            some_limit,
            full_limit,
        } => format!(
            "swap would have to cover {gap_mib} MiB but memory pressure is too high \
             (some {some_avg10:?}% / {some_limit}%, full {full_avg10:?}% / {full_limit}%)"
        ),
        MemoryRefusal::ClaimExceedsHostCapacity {
            required_mib,
            ram_mib,
        } => format!(
            "the claim ({required_mib} MiB) exceeds this host's physical RAM ({ram_mib} MiB); \
             the claim is pinned, so swap cannot make it fit"
        ),
    }
}
