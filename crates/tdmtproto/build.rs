//! Generates the wire-scheme Rust modules into OUT_DIR (S10 telegram_api,
//! S13 secret_api + mtproto_api). The schemes are embedded in the
//! tl-codegen crate; regeneration is keyed on its sources.

use std::path::Path;

fn main() {
    let generator = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tl-codegen");
    // Per-file rerun-if-changed: a directory mtime does not change when a
    // file inside it is edited.
    for source in ["src/lib.rs", "src/model.rs", "src/rust_writer.rs"] {
        println!(
            "cargo:rerun-if-changed={}",
            generator.join(source).display()
        );
    }
    println!(
        "cargo:rerun-if-changed={}",
        generator.join("scheme").display()
    );
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir);
    let code = tl_codegen::generate_telegram_api_rust();
    std::fs::write(out_dir.join("telegram_api.rs"), code).unwrap();
    let code = tl_codegen::generate_secret_api_rust();
    std::fs::write(out_dir.join("secret_api.rs"), code).unwrap();
    let code = tl_codegen::generate_mtproto_api_rust();
    std::fs::write(out_dir.join("mtproto_api.rs"), code).unwrap();
}
