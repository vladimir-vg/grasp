//! `grasp compile <program.grasp> [-o <program.gdbsp>]`.
//!
//! What turns a grasp program into something `grasp-dbsp-server` can run. The
//! arguments are read from `std::env::args` by hand: this crate's dependencies
//! are deliberately empty ("text in, text out"), and one command with one flag
//! does not justify a parser.

use std::process::ExitCode;

const USAGE: &str = "usage: grasp compile <program.grasp> [-o <program.gdbsp>]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let [command, rest @ ..] = args else {
        return Err(USAGE.to_string());
    };
    if command != "compile" {
        return Err(format!("unknown command `{command}`\n{USAGE}"));
    }
    let (input, output) = match rest {
        [input] => (input, None),
        [input, flag, output] if flag == "-o" => (input, Some(output)),
        _ => return Err(USAGE.to_string()),
    };
    let source = std::fs::read_to_string(input).map_err(|e| format!("reading `{input}`: {e}"))?;
    let emitted = grasp_compiler::compile(&source).map_err(|d| grasp_compiler::diag::render(&d))?;
    match output {
        Some(path) => std::fs::write(path, emitted).map_err(|e| format!("writing `{path}`: {e}")),
        // Standard output when no file is named, so it composes with a pipe.
        None => {
            print!("{emitted}");
            Ok(())
        }
    }
}
