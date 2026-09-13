//! Port of `benchmark/wget.cpp` — fetch a URL through the ported `Wget`.
//!
//! Usage: `cargo run -p tdnet --example wget [url] [-6]`
//! Defaults to `https://telegram.org` with a 10s timeout and ttl 3.

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("https://telegram.org");
    let prefer_ipv6 = args.get(2).map(|arg| arg == "-6").unwrap_or(false);

    let result = tdnet::wget::wget(
        url,
        tdnet::wget::WgetParams {
            prefer_ipv6,
            ..tdnet::wget::WgetParams::default()
        },
    )
    .await;
    match result {
        tdutils::status::Result::Ok(query) => {
            println!("{} {}", query.code, query.reason);
            for (name, value) in &query.headers {
                println!("{name}: {value}");
            }
            println!();
            println!("{}", String::from_utf8_lossy(&query.body));
        }
        tdutils::status::Result::Err(status) => {
            eprintln!("error: {}", status.message());
            std::process::exit(1);
        }
    }
}
