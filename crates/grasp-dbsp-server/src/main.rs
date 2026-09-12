//! The command line.

use clap::{Parser, Subcommand};
use grasp_dbsp_runner::diag::render;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "grasp-dbsp-server", about = "Run a grasp-dbsp program.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile a program and a configuration, and report what is wrong with
    /// either. Nothing is started.
    Validate {
        /// The program, conventionally `.gdbsp`.
        program: PathBuf,
        /// The pipeline configuration. Optional, because a program can be
        /// wrong on its own.
        #[arg(long = "config-file")]
        config_file: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Validate {
            program,
            config_file,
        } => validate(&program, config_file.as_deref()),
    }
}

/// Both halves are checked even when the first fails, because a person fixing a
/// configuration wants every problem at once rather than one per run.
fn validate(program: &std::path::Path, config_file: Option<&std::path::Path>) -> ExitCode {
    let mut failed = false;
    // Kept so the configuration can be checked against the program: a
    // `materialized:` entry naming a view the program does not have is only
    // visible with both in hand.
    let mut plan_for_config = None;

    match std::fs::read_to_string(program) {
        Err(e) => {
            eprintln!("reading `{}`: {e}", program.display());
            failed = true;
        }
        Ok(source) => match grasp_dbsp_runner::compile(&source) {
            Err(diags) => {
                eprintln!("{}", render(&diags));
                failed = true;
            }
            Ok(plan) => {
                let views = plan.views();
                plan_for_config = Some(plan.clone());
                let inputs: Vec<&str> = plan.inputs().into_iter().map(|(_, t)| t).collect();
                println!("{}: {} nodes", program.display(), plan.nodes.len());
                println!("  tables: {}", list(&inputs));
                println!("  views:  {}", list(&views));
            }
        },
    }

    if let Some(path) = config_file {
        match grasp_dbsp_server::config::PipelineConfig::read(path) {
            Err(diags) => {
                eprintln!("{}", render(&diags));
                failed = true;
            }
            Ok(config) => match config.runner() {
                Err(diags) => {
                    eprintln!("{}", render(&diags));
                    failed = true;
                }
                Ok(runner) => {
                    if let Some(plan) = &plan_for_config
                        && let Err(diags) = config.check_against(plan)
                    {
                        eprintln!("{}", render(&diags));
                        failed = true;
                    }
                    println!("{}: pipeline `{}`", path.display(), config.name);
                    println!("  workers: {}", runner.workers);
                    println!(
                        "  storage: {}",
                        if runner.storage.is_some() {
                            "on"
                        } else {
                            "in memory"
                        }
                    );
                    if !config.materialized.is_empty() {
                        println!("  materialized: {}", list(&config.materialized));
                    }
                }
            },
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn list<S: AsRef<str>>(items: &[S]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items
            .iter()
            .map(|s| s.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
