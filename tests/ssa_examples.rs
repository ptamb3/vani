//! Cross-example check that the SSA lowerer accepts every
//! .intent file in examples/. Catches "feature X used in
//! example Y broke the lowerer" regressions early.

use std::fs;

use vani::compile;
use vani::ssa::{lower_program, LowerError};

#[test]
fn ssa_lowers_every_example() {
    let dir = format!("{}/examples", env!("CARGO_MANIFEST_DIR"));
    let mut failures: Vec<(String, Vec<LowerError>)> = Vec::new();
    let entries = fs::read_dir(&dir).expect("examples dir exists");
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("intent") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("read example");
        let checked = match compile(&source) {
            Ok(c) => c,
            Err(diags) => {
                // The example doesn't type-check — that's a
                // bug elsewhere, not the SSA lowerer's
                // concern. Skip rather than fail this test.
                eprintln!(
                    "warning: {} did not type-check ({} diagnostics); skipping SSA check",
                    path.display(),
                    diags.len()
                );
                continue;
            }
        };
        let (_module, errors) = lower_program(&checked.ir);
        // Gated errors carry a "not yet supported" marker —
        // the tree backend handles those examples instead.
        // The test only fails on *unexpected* SSA errors so
        // new gated features (structs, enums, match, …)
        // don't require manual skip-list maintenance here.
        let unexpected: Vec<LowerError> = errors
            .into_iter()
            .filter(|e| !e.message.contains("not yet supported"))
            .collect();
        if !unexpected.is_empty() {
            failures.push((path.display().to_string(), unexpected));
        }
    }
    if !failures.is_empty() {
        let mut msg = String::from("SSA lowerer rejected some examples:\n");
        for (path, errs) in &failures {
            msg.push_str(&format!("  {}:\n", path));
            for e in errs {
                msg.push_str(&format!("    - {}\n", e));
            }
        }
        panic!("{}", msg);
    }
}
