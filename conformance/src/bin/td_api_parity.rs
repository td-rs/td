//! S84 parity-audit counter: tracks the plan's progress metric —
//! "functions implemented / 1,022 × conformance sessions green".
//!
//! Parses the pinned `td_api.tl` through `tdtl`/`tl-codegen` (the same
//! front end the codegen uses) to get the authoritative function list,
//! then scans the in-repo crate sources for `Function::<name>` dispatch
//! references (the handler wiring Track I adds manager by manager).
//! Emits the count, the implemented names, and the not-yet-implemented
//! remainder so a release's parity audit is one command.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "target" | "target-clippy" | ".git") {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn main() {
    // 1. The authoritative function list from the pinned scheme.
    let program = tdtl::TlProgram::parse(tl_codegen::TD_API_TL.as_bytes().to_vec())
        .expect("td_api.tl parses");
    let all: BTreeSet<String> = program.func_ids.keys().cloned().collect();
    let total = all.len();
    assert_eq!(
        total, 1022,
        "td_api function count drifted from the S03 pin"
    );

    // 2. Handler references across the in-repo crate sources.
    let mut sources = Vec::new();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crates");
    collect_rs_files(&root, &mut sources);
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for file in &sources {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let mut rest = text.as_str();
        while let Some(pos) = rest.find("Function::") {
            rest = &rest[pos + "Function::".len()..];
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !ident.is_empty() {
                referenced.insert(ident);
            }
        }
    }

    let implemented: Vec<&String> = all.iter().filter(|f| referenced.contains(*f)).collect();
    let missing = total - implemented.len();

    println!("td_api functions: {total} (pinned scheme)");
    println!(
        "handler references found: {} ({} distinct)",
        implemented.len(),
        implemented.len()
    );
    println!(
        "parity: {}/{} = {:.1}%",
        implemented.len(),
        total,
        implemented.len() as f64 / total as f64 * 100.0
    );
    if !implemented.is_empty() {
        println!("\nimplemented:");
        for name in &implemented {
            println!("  {name}");
        }
    }
    if missing <= 40 {
        println!("\nnot yet implemented:");
        for name in all.iter().filter(|f| !referenced.contains(*f)) {
            println!("  {name}");
        }
    }
}
