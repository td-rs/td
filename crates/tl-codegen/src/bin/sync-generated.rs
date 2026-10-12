//! Writes the generated scheme modules into the consuming crates'
//! `src/generated/` trees. The runtime crates (tdclient, tdmtproto, tde2e)
//! compile those committed files directly — tl-codegen is a dev tool, not
//! a build dependency — so this is the only step that regenerates them:
//!
//!     cargo run -p tl-codegen --bin sync-generated
//!
//! Outputs are written only when their bytes change, so a sync that
//! produces identical code leaves the consuming crates' builds fresh:
//! their rebuild is keyed on the generated files' mtimes, exactly like an
//! edit to hand-written source. The `generated_drift` test fails CI when
//! the committed files no longer match the generator — run this bin after
//! changing the schemes, the model, or the writer.

use std::path::{Path, PathBuf};

/// One regeneration target: the committed generated file (relative to the
/// crate root) and the writer that regenerates it.
type Target = (&'static str, fn() -> String);

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let targets: [Target; 5] = [
        (
            "../tdclient/src/generated/td_api.rs",
            tl_codegen::generate_td_api_rust,
        ),
        (
            "../tdmtproto/src/generated/telegram_api.rs",
            tl_codegen::generate_telegram_api_rust,
        ),
        (
            "../tdmtproto/src/generated/secret_api.rs",
            tl_codegen::generate_secret_api_rust,
        ),
        (
            "../tdmtproto/src/generated/mtproto_api.rs",
            tl_codegen::generate_mtproto_api_rust,
        ),
        (
            "../tde2e/src/generated/e2e_api.rs",
            tl_codegen::generate_e2e_api_rust,
        ),
    ];
    for (rel, generate) in targets {
        let path = root.join(rel);
        let code = generate();
        match write_if_changed(&path, &code) {
            true => println!("wrote {}", path.display()),
            false => println!("unchanged {}", path.display()),
        }
    }
}

/// Writes `contents` to `path` only when the bytes differ (or the file
/// does not exist yet), keeping the mtime stable across syncs that
/// produce identical output. Returns whether the file was written.
fn write_if_changed(path: &PathBuf, contents: &str) -> bool {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return false;
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    true
}
