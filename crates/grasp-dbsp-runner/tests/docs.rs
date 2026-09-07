//! grasp-dbsp code that appears in `docs/grasp/` compiles.
//!
//! `docs/grasp/mapping.md` shows what `grasp-compiler` is supposed to emit. That
//! makes its examples a claim about *this* crate's language, and a claim only
//! this crate can check — which is why the test lives here rather than beside
//! the compiler, which cannot yet emit anything.
//!
//! It is the same guarantee `reserved.rs` gives `docs/grasp-dbsp/language.md`:
//! an example that does not compile teaches a program that does not compile.

/// The worked example in `mapping.md` compiles.
///
/// This is the one block in that document which is a whole program; the others
/// are fragments that omit their input declarations, and are covered by the
/// constructs this one exercises — `input`, `map`, `map_index`, `join`, `plus`,
/// `circuit` and `fixpoint`.
#[test]
fn the_mapping_example_compiles() {
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/grasp/mapping.md");
    let doc = std::fs::read_to_string(&doc).expect("mapping.md");
    // Splitting on the fence gives prose at even indices and code at odd ones.
    let block = doc
        .split("```")
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, b)| b)
        .find(|b| b.contains("circuit path_scc") && b.contains("fixpoint("))
        .expect("the worked example");
    if let Err(diags) = grasp_dbsp_runner::compile(block.trim_start_matches('\n')) {
        panic!(
            "the worked example in docs/grasp/mapping.md does not compile: {}",
            diags
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
}
