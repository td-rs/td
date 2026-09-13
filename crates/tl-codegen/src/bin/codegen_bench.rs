//! S17 baseline: regenerates every scheme in-process and prints the wall
//! time and output size of each generated module. The recorded numbers live
//! in `bench/codegen-baseline.md`; rerun this bin (release) to compare
//! against the baseline after generator changes.

use std::time::Instant;

use tl_codegen::{
    generate_e2e_api_rust, generate_mtproto_api_rust, generate_secret_api_rust,
    generate_td_api_rust, generate_telegram_api_rust,
};

fn measure(name: &str, generator: fn() -> String) {
    let start = Instant::now();
    let code = generator();
    let elapsed = start.elapsed();
    println!(
        "{name}: {:>8.3}s {:>10} bytes {:>8} lines",
        elapsed.as_secs_f64(),
        code.len(),
        code.lines().count()
    );
}

fn main() {
    measure("td_api", generate_td_api_rust);
    measure("telegram_api", generate_telegram_api_rust);
    measure("secret_api", generate_secret_api_rust);
    measure("mtproto_api", generate_mtproto_api_rust);
    measure("e2e_api", generate_e2e_api_rust);
}
