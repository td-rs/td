// The S74 replayer: consumes the S73 recorder corpus (one request JSON per
// line) and produces the same line-delimited record stream through the
// Rust tdclient layer — `{"request": ...}` for each request,
// `{"update": ...}` for each output (updates and responses alike, drained
// until a 0.5 s idle). `recorder/verify.sh` diffs this stream against the
// C++ recorder's output modulo the documented nondeterminism.
//
// The live corpus linger (TD_RECORD_LINGER_MS, shared with the C++
// recorder): the S76 echo of the replayed sendMessage arrives a beat after
// the request drain goes idle — keep receiving until the window elapses
// (0 for the offline corpus).

use std::time::{Duration, Instant};

use tdclient::client_json::ClientJson;

fn main() {
    let linger = std::env::var("TD_RECORD_LINGER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0u64);

    let client = ClientJson::new();
    for line in std::io::stdin().lines() {
        let Ok(request) = line else { break };
        if request.is_empty() {
            break;
        }
        // A `{"@wait":"<@type>"}` directive sends nothing: drain until the
        // update with the given @type arrives (then to idle) — the C++
        // recorder's counterpart for the async completion windows.
        if request.starts_with("{\"@wait\":") {
            println!("{{\"request\": {request}}}");
            let cond = request
                .split("\"@wait\":\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default();
            let needle = format!("\"@type\":\"{cond}\"");
            let mut matched = false;
            for _ in 0..1200 {
                match client.receive(0.25) {
                    Some(response) => {
                        println!("{{\"update\": {response}}}");
                        if !cond.is_empty() && response.contains(&needle) {
                            matched = true;
                            break;
                        }
                    }
                    None => {}
                }
            }
            if !matched {
                eprintln!("wait timed out for {cond}");
                std::process::exit(1);
            }
            for _ in 0..40 {
                match client.receive(0.25) {
                    Some(response) => println!("{{\"update\": {response}}}"),
                    None => break,
                }
            }
            continue;
        }
        client.send(&request);
        println!("{{\"request\": {request}}}");

        // Drain until the queue is idle (the C++ recorder's 0.5 s probe).
        for _ in 0..100 {
            match client.receive(0.5) {
                Some(response) => println!("{{\"update\": {response}}}"),
                None => break,
            }
        }
    }

    if linger > 0 {
        let deadline = Instant::now() + Duration::from_millis(linger);
        while Instant::now() < deadline {
            match client.receive(0.1) {
                Some(response) => println!("{{\"update\": {response}}}"),
                None => {}
            }
        }
    }
}
