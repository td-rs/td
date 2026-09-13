//! Generates the td_api Rust module into OUT_DIR (S09). The schemes are
//! embedded in the tl-codegen crate, so regeneration is keyed on its
//! sources rather than on scheme paths.

use std::path::Path;

fn main() {
    let generator = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tl-codegen");
    println!("cargo:rerun-if-changed={}", generator.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        generator.join("scheme").display()
    );
    let code = tl_codegen::generate_td_api_rust();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(Path::new(&out_dir).join("td_api.rs"), code).unwrap();
}
