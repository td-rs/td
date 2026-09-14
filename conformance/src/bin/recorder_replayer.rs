// The S74 replayer: consumes the S73 recorder corpus (one request JSON per
// line) and produces the same line-delimited record stream through the
// Rust tdclient layer — `{"request": ...}` for each request,
// `{"update": ...}` for each output (updates and responses alike, drained
// until a 0.5 s idle). `recorder/verify.sh` diffs this stream against the
// C++ recorder's output modulo the documented nondeterminism.

use tdclient::client_json::ClientJson;

fn main() {
    let client = ClientJson::new();
    for line in std::io::stdin().lines() {
        let Ok(request) = line else { break };
        if request.is_empty() {
            break;
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
}
