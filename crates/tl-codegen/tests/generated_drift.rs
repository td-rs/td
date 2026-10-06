//! CI gate: the committed `src/generated/` modules in the consuming
//! crates must match the generator exactly — they are the build-time
//! interface between tl-codegen and the runtime crates (the crates
//! compile the committed files directly; tl-codegen is not a build
//! dependency). Regenerate with
//! `cargo run -p tl-codegen --bin sync-generated` after changing the
//! schemes, the model, or the writer.

use std::path::Path;

fn check(rel: &str, generated: String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    if committed != generated {
        let (line, col) = mismatch_at(&committed, &generated);
        panic!(
            "{} is stale: first difference at line {line} col {col} \
             (committed {} bytes, generator {} bytes) — \
             run `cargo run -p tl-codegen --bin sync-generated`",
            path.display(),
            committed.len(),
            generated.len(),
        );
    }
}

fn mismatch_at(a: &str, b: &str) -> (usize, usize) {
    let bytes = a.bytes().zip(b.bytes()).position(|(x, y)| x != y);
    let prefix = match bytes {
        Some(n) => &a[..n],
        None => return (a.lines().count(), 0),
    };
    let line = prefix.matches('\n').count() + 1;
    let col = prefix.rsplit('\n').next().map_or(0, |l| l.len() + 1);
    (line, col)
}

#[test]
fn generated_schemes_match_generator() {
    check(
        "../tdclient/src/generated/td_api.rs",
        tl_codegen::generate_td_api_rust(),
    );
    check(
        "../tdmtproto/src/generated/telegram_api.rs",
        tl_codegen::generate_telegram_api_rust(),
    );
    check(
        "../tdmtproto/src/generated/secret_api.rs",
        tl_codegen::generate_secret_api_rust(),
    );
    check(
        "../tdmtproto/src/generated/mtproto_api.rs",
        tl_codegen::generate_mtproto_api_rust(),
    );
    check(
        "../tde2e/src/generated/e2e_api.rs",
        tl_codegen::generate_e2e_api_rust(),
    );
}
