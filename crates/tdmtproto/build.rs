//! Generates the telegram_api Rust module into OUT_DIR (S10). The scheme is
//! embedded in the tl-codegen crate; regeneration is keyed on its sources.

use std::path::Path;

fn main() {
    let generator = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tl-codegen");
    println!("cargo:rerun-if-changed={}", generator.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        generator.join("scheme").display()
    );
    let code = tl_codegen::generate_telegram_api_rust();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(Path::new(&out_dir).join("telegram_api.rs"), code).unwrap();
}
