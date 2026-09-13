//! The `grasp` command line, run as a process.

use std::process::{Command, Output};

fn grasp(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grasp"))
        .args(args)
        .output()
        .expect("the binary runs")
}

/// A file in the system temporary directory, named for this process so that
/// parallel runs do not collide.
fn scratch(name: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("grasp-cli-{}-{name}", std::process::id()));
    std::fs::write(&path, contents).expect("writes the program");
    path
}

#[test]
fn compile_writes_grasp_dbsp_that_grasp_dbsp_accepts() {
    let program = scratch(
        "ok.grasp",
        "edge :: relation(src: i64, dst: i64)\nedge(src:, dst:) <- input\n",
    );
    let out = program.with_extension("gdbsp");
    let run = grasp(&[
        "compile",
        program.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let text = std::fs::read_to_string(&out).expect("the output file");
    if let Err(d) = grasp_dbsp::compile(&text) {
        panic!(
            "grasp-dbsp rejected the output: {}\n{text}",
            grasp_dbsp::diag::render(&d)
        );
    }
}

#[test]
fn a_program_that_does_not_compile_exits_non_zero_with_its_diagnostics() {
    let program = scratch("bad.grasp", "q(x: y) <- nope(x: y)\n");
    let run = grasp(&["compile", program.to_str().unwrap()]);
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("nope"),
        "the diagnostic names the problem: {stderr}"
    );
}

#[test]
fn without_a_command_it_says_how_to_use_it() {
    let run = grasp(&[]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("usage: grasp compile"));
}
