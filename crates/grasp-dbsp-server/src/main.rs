//! The command line.

use clap::{Parser, Subcommand};
use grasp_dbsp::diag::render;
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
    /// Run the program and serve it over Feldera's HTTP API.
    Serve {
        /// The program, conventionally `.gdbsp`.
        program: PathBuf,
        #[arg(long = "config-file")]
        config_file: Option<PathBuf>,
        /// The address to listen on.
        #[arg(long, default_value = "127.0.0.1")]
        bind_address: String,
        /// The port. Zero asks the operating system for one, which is then
        /// printed — Feldera does the same and writes it to a file.
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Start paused, as Feldera's `--initial=paused` does. A paused
        /// pipeline accepts rows and does not compute them.
        #[arg(long)]
        paused: bool,
    },
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
        Command::Serve {
            program,
            config_file,
            bind_address,
            port,
            paused,
        } => match serve(
            &program,
            config_file.as_deref(),
            &bind_address,
            port,
            !paused,
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("{message}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Builds everything that can fail, then listens.
///
/// The order matters and is the whole reason this is not inline in `main`:
/// the program is compiled, the configuration is read, the storage backend is
/// opened and the circuit is built **before** a port is bound. A program that
/// does not compile, a configuration with a bad key, or a storage directory
/// another pipeline holds is then a diagnostic on stderr and a non-zero exit —
/// not a server that accepts connections and cannot answer them.
fn serve(
    program: &std::path::Path,
    config_file: Option<&std::path::Path>,
    bind_address: &str,
    port: u16,
    running: bool,
) -> Result<(), String> {
    use grasp_dbsp_server::{circuit, config::PipelineConfig, http};
    use std::collections::HashMap;

    let source = std::fs::read_to_string(program)
        .map_err(|e| format!("reading `{}`: {e}", program.display()))?;
    let plan = grasp_dbsp::compile(&source).map_err(|d| render(&d))?;

    let config = match config_file {
        Some(path) => PipelineConfig::read(path).map_err(|d| render(&d))?,
        None => PipelineConfig::parse("{}").map_err(|d| render(&d))?,
    };
    config.check_against(&plan).map_err(|d| render(&d))?;
    let runner_config = config.runner().map_err(|d| render(&d))?;

    // Every declared name is a view. Nodes that are not selected are built
    // anyway — there is no dead-code elimination — so the cost of selecting
    // them all is an accumulator and a sink each, and the benefit is that
    // `/egress/{anything the program named}` works without configuration.
    let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
    let runner =
        grasp_dbsp::lower::Runner::build(&plan, &views, runner_config).map_err(|d| render(&d))?;

    let shapes: HashMap<String, grasp_dbsp::value::BatchType> = views
        .iter()
        .filter_map(|v| grasp_dbsp::lower::shape(&plan, v).map(|t| (v.clone(), t.clone())))
        .collect();
    let tables: Vec<String> = plan
        .inputs()
        .into_iter()
        .map(|(_, t)| t.to_string())
        .collect();

    let (handle, thread) = circuit::start(runner, shapes, &config.materialized, tables, running);

    let name = config.name.clone();
    eprintln!(
        "grasp-dbsp-server: pipeline `{name}` on http://{bind_address}:{port}, \
         {} table(s), {} view(s), {}",
        handle.tables.len(),
        handle.views.len(),
        if running { "running" } else { "paused" }
    );

    let result = actix_web::rt::System::new().block_on(async move {
        let state = actix_web::web::Data::new(http::State {
            handle,
            pipeline: name,
            keepalive: std::time::Duration::from_secs(3),
        });
        actix_web::HttpServer::new(move || {
            actix_web::App::new()
                .app_data(state.clone())
                .configure(http::configure)
        })
        .bind((bind_address, port))?
        .run()
        .await
    });

    // Joining is what releases the storage directory's lock. Skipping it — by
    // exiting the process here — would leave the next start waiting sixty
    // seconds for a pidlock nobody holds.
    let _ = thread.join();
    result.map_err(|e| format!("serving: {e}"))
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
        Ok(source) => match grasp_dbsp::compile(&source) {
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
