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
    // Everything drained since the last real request: an @wait directive
    // must also match updates that already arrived in the previous drain
    // (a state can complete inside the prior request's drain window —
    // the code-check → WaitPassword transition does this when the server
    // is fast), not only updates that arrive while the wait is watching.
    // Mirrors record.cpp exactly.
    let mut drained_since_request = String::new();
    for line in std::io::stdin().lines() {
        let Ok(request) = line else { break };
        if request.is_empty() {
            break;
        }
        // A `{"@idle":N}` directive sends nothing: drain until the stream
        // has been silent for N consecutive seconds (quiescence) — the
        // config fan-out window pinner for the live user-path corpus.
        // Mirrors record.cpp; the drain also feeds
        // drained_since_request for later @wait matches.
        if let Some(rest) = request.strip_prefix("{\"@idle\":") {
            println!("{{\"request\": {request}}}");
            let idle_secs: f64 = rest.trim_end_matches('}').parse().unwrap_or(1.0);
            let deadline = Instant::now() + Duration::from_secs_f64(idle_secs.clamp(0.0, 120.0));
            let mut quiet = 0.0f64;
            while Instant::now() < deadline && quiet < idle_secs {
                match client.receive(0.25) {
                    Some(response) => {
                        quiet = 0.0;
                        println!("{{\"update\": {response}}}");
                        drained_since_request.push_str(&response);
                    }
                    None => quiet += 0.25,
                }
            }
            continue;
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
            let mut matched = cond.is_empty() || drained_since_request.contains(&needle);
            let mut probes = 0u32;
            while !matched && probes < 1200 {
                match client.receive(0.25) {
                    Some(response) => {
                        println!("{{\"update\": {response}}}");
                        drained_since_request.push_str(&response);
                        if !cond.is_empty() && response.contains(&needle) {
                            matched = true;
                            break;
                        }
                    }
                    None => {}
                }
                probes += 1;
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
        drained_since_request.clear();
        client.send(&request);
        println!("{{\"request\": {request}}}");

        // Drain until the queue is idle (the C++ recorder's 0.5 s probe).
        for _ in 0..100 {
            match client.receive(0.5) {
                Some(response) => {
                    println!("{{\"update\": {response}}}");
                    drained_since_request.push_str(&response);
                }
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
