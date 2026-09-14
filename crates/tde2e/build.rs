//! Generates the e2e_api Rust module into OUT_DIR (S13). The scheme is
//! embedded in the tl-codegen crate; regeneration is keyed on its sources.

use std::path::Path;

fn main() {
    let generator = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tl-codegen");
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
    let code = tl_codegen::generate_e2e_api_rust();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(Path::new(&out_dir).join("e2e_api.rs"), code).unwrap();
}
