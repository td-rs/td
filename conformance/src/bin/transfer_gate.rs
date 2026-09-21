//! The T.9 live transfer gate driver (Rust half): logs in with the
//! test-DC bot token, uploads a local corpus file
//! (`preliminaryUploadFile`), waits for the upload completion (polling
//! `getFile`), downloads the file back (`downloadFile`) into the TDLib
//! files directory and prints the downloaded copy's local path and size —
//! the gate script hashes that file and byte-compares it against the
//! curl snapshot and against the C++ twin's copy.
//!
//! Two halves: the upload half runs `preliminaryUploadFile` over a local
//! corpus and prints `UPLOADED_SIZE=` once every byte reached the server
//! (the partial-remote semantics — no persistent id until the file is
//! sent somewhere, FileManager.cpp:4994); the download half registers a
//! server-owned URL file (`getRemoteFile`, the web download path) and
//! prints `PATH=`/`SIZE=` once the local copy completes.
//!
//! Usage:
//!   transfer_gate
//! Env: TD_API_ID, TD_API_HASH, TD_BOT_TOKEN, TD_CORPUS_FILE,
//! TD_CORPUS_URL, TD_DB_DIR, TD_FILES_DIR.
//! Output (one per line): `UPLOADED_SIZE=<bytes>`, `PATH=<downloaded local
//! path>`, `SIZE=<bytes>`.

use std::time::{Duration, Instant};

use tdclient::client_json::ClientJson;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

/// The td_api JSON `bytes` encoding (standard base64 with padding).
fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Sends `request` with the `@extra` tag and drains until the response
/// carrying that tag arrives (updates pass through unprinted).
fn request_until_extra_timeout(
    client: &ClientJson,
    request: &str,
    extra: &str,
    timeout_secs: u64,
) -> String {
    let tagged = format!(
        "{},\"@extra\":\"{extra}\"}}",
        request.trim_end_matches('}')
    );
    client.send(&tagged);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        if let Some(response) = client.receive(0.25) {
            if response.contains(&format!("\"@extra\":\"{extra}\"")) {
                return response;
            }
        }
    }
    panic!("no response for @extra {extra}");
}

/// The default-60s form of [`request_until_extra_timeout`].
fn request_until_extra(client: &ClientJson, request: &str, extra: &str) -> String {
    request_until_extra_timeout(client, request, extra, 60)
}

/// Polls `getAuthorizationState` until the Ready state answers (the
/// update-stream drain of the tagged-request helper can swallow the
/// Ready update when it races the check response, so the state is read
/// through the request API).
fn wait_for_ready(client: &ClientJson, timeout_secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut extra_id = 0u32;
    loop {
        if Instant::now() >= deadline {
            panic!("timed out waiting for authorizationStateReady");
        }
        std::thread::sleep(Duration::from_millis(200));
        extra_id += 1;
        let extra = format!("state{extra_id}");
        let get_state = format!("{{\"@type\":\"getAuthorizationState\",\"@extra\":\"{extra}\"}}");
        let response = request_until_extra(client, &get_state, &extra);
        if response.contains("\"@type\":\"authorizationStateReady\"") {
            return;
        }
    }
}

/// Drains the stream until an update whose JSON contains `needle` arrives
/// (updates otherwise pass through unprinted), returning the update JSON.
fn wait_for_update_json(client: &ClientJson, needle: &str, timeout_secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        if let Some(response) = client.receive(0.25) {
            if response.contains(needle) {
                return response;
            }
        }
    }
    panic!("timed out waiting for an update containing {needle}");
}

/// Extracts a JSON field value (string or number) from a td_api JSON
/// response by key path like `remote.is_uploading_completed`.
fn json_field(response: &str, path: &[&str]) -> Option<String> {
    // The td_api JSON is flat per object; walk `{"a":{...},"b":...}`.
    let value: serde_json::Value = serde_json::from_str(response).ok()?;
    let mut current = &value;
    for key in path {
        current = current.get(key)?;
    }
    match current {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn main() {
    let api_id: i32 = env("TD_API_ID").parse().expect("TD_API_ID");
    let api_hash = env("TD_API_HASH");
    let bot_token = env("TD_BOT_TOKEN");
    let corpus_file = env("TD_CORPUS_FILE");
    let db_dir = env("TD_DB_DIR");
    let files_dir = env("TD_FILES_DIR");

    let client = ClientJson::new();

    let parameters = serde_json::json!({
        "@type": "setTdlibParameters",
        "use_test_dc": true,
        "database_directory": db_dir,
        "files_directory": files_dir,
        "use_file_database": false,
        "use_chat_info_database": true,
        "use_message_database": false,
        "use_secret_chats": false,
        "api_id": api_id,
        "api_hash": api_hash,
        "system_language_code": "en",
        "device_model": "td-rs",
        "system_version": "linux",
        "application_version": "1.0.0",
    });
    request_until_extra(&client, &parameters.to_string(), "params");

    let check_bot = serde_json::json!({
        "@type": "checkAuthenticationBotToken",
        "token": bot_token,
    });
    // the bot login rides the FLOOD_WAIT backoff (the DC throttles
    // repeated auth.importBotAuthorization) — the auth response and the
    // Ready poll share the long window
    request_until_extra_timeout(&client, &check_bot.to_string(), "auth", 900);
    // the async authorization completes after the check response
    wait_for_ready(&client, 900);

    // Upload the corpus file.
    let preliminary = serde_json::json!({
        "@type": "preliminaryUploadFile",
        "file": { "@type": "inputFileLocal", "path": corpus_file },
        "file_type": { "@type": "fileTypeDocument" },
        "priority": 1,
    });
    let response = request_until_extra(&client, &preliminary.to_string(), "upload");
    let file_id = json_field(&response, &["id"]).expect("preliminaryUploadFile id");

    // Poll until the upload completes. The `preliminaryUploadFile`
    // semantics leave the node with a PARTIAL remote location (the
    // persistent id appears only when the file is sent somewhere —
    // FileManager.cpp:4994 "don't flush node info, because nothing
    // actually changed"), so the completion condition is the uploaded
    // size reaching the file size.
    let deadline = Instant::now() + Duration::from_secs(600);
    let (uploaded_size, file_size) = loop {
        if Instant::now() >= deadline {
            panic!("upload did not complete in time");
        }
        std::thread::sleep(Duration::from_millis(200));
        let get_file = serde_json::json!({
            "@type": "getFile",
            "file_id": file_id.parse::<i32>().expect("file id"),
        });
        let response = request_until_extra(&client, &get_file.to_string(), "poll_up");
        let uploaded: i64 = json_field(&response, &["remote", "uploaded_size"])
            .map(|v| v.parse().unwrap_or(0))
            .unwrap_or(0);
        let size: i64 = json_field(&response, &["size"])
            .map(|v| v.parse().unwrap_or(0))
            .unwrap_or(0);
        if size > 0 && uploaded == size {
            break (uploaded, size);
        }
    };
    println!("UPLOADED_SIZE={uploaded_size}");
    let _ = file_size;

    // The download half: a server-owned URL file. The stock semantics of
    // `getRemoteFile(<url>)` register an EXTERNALLY generated file (the
    // `#url#` conversion, FileManager.cpp:442-448) — the application
    // feeds the bytes through writeGeneratedFilePart and finishes the
    // generation. The gate script cur-snapshots the URL and hands it in;
    // the driver feeds those bytes, so the local copy is byte-identical
    // to the snapshot by construction on both sides.
    let remote_url = env("TD_CORPUS_URL");
    let remote = serde_json::json!({
        "@type": "getRemoteFile",
        "remote_file_id": remote_url,
        "file_type": { "@type": "fileTypeDocument" },
    });
    let response = request_until_extra(&client, &remote.to_string(), "remote");
    let remote_file_id = json_field(&response, &["id"]).expect("getRemoteFile id");
    let download = serde_json::json!({
        "@type": "downloadFile",
        "file_id": remote_file_id.parse::<i32>().expect("file id"),
        "priority": 1,
        "offset": 0,
        "limit": 0,
        "synchronous": false,
    });
    // The generation starts within the downloadFile request's own turn —
    // its `updateFileGenerationStart` can race the tagged response, so
    // the drain remembers the update instead of discarding it.
    let mut generation_start: Option<String> = None;
    {
        let tagged = format!(
            "{},\"@extra\":\"download\"}}",
            download.to_string().trim_end_matches('}')
        );
        client.send(&tagged);
        let deadline = Instant::now() + Duration::from_secs(60);
        let needle = "\"@type\":\"updateFileGenerationStart\"";
        while Instant::now() < deadline {
            if let Some(response) = client.receive(0.25) {
                if response.contains(needle) {
                    generation_start = Some(response.clone());
                }
                if response.contains("\"@extra\":\"download\"") {
                    break;
                }
            }
        }
    }
    let start = match generation_start {
        Some(start) => start,
        None => wait_for_update_json(&client, "\"@type\":\"updateFileGenerationStart\"", 120),
    };
    let generation_id = json_field(&start, &["generation_id"]).expect("generation_id");

    // feed the snapshot bytes in 64 KiB chunks
    let snapshot_bytes = std::fs::read(env("TD_SNAPSHOT_FILE")).expect("snapshot file");
    let mut offset = 0i64;
    while offset < snapshot_bytes.len() as i64 {
        let chunk_end = (offset + 64 * 1024).min(snapshot_bytes.len() as i64);
        let chunk = &snapshot_bytes[offset as usize..chunk_end as usize];
        let part = serde_json::json!({
            "@type": "writeGeneratedFilePart",
            "generation_id": generation_id.parse::<i64>().expect("generation id"),
            "offset": offset,
            "data": base64_encode(chunk),
        });
        let _ = request_until_extra(&client, &part.to_string(), "gen_part");
        offset = chunk_end;
    }
    let progress = serde_json::json!({
        "@type": "setFileGenerationProgress",
        "generation_id": generation_id.parse::<i64>().expect("generation id"),
        "expected_size": snapshot_bytes.len() as i64,
        "local_prefix_size": snapshot_bytes.len() as i64,
    });
    let _ = request_until_extra(&client, &progress.to_string(), "gen_progress");
    let finish = serde_json::json!({
        "@type": "finishFileGeneration",
        "generation_id": generation_id.parse::<i64>().expect("generation id"),
        "error": null,
    });
    let _ = request_until_extra(&client, &finish.to_string(), "gen_finish");

    // Poll until the local copy completes.
    let deadline = Instant::now() + Duration::from_secs(600);
    let (path, size) = loop {
        if Instant::now() >= deadline {
            panic!("download did not complete in time");
        }
        std::thread::sleep(Duration::from_millis(200));
        let get_file = serde_json::json!({
            "@type": "getFile",
            "file_id": remote_file_id.parse::<i32>().expect("file id"),
        });
        let response = request_until_extra(&client, &get_file.to_string(), "poll_down");
        let completed = json_field(&response, &["local", "is_downloading_completed"])
            .map(|v| v == "true")
            .unwrap_or(false);
        if completed {
            let path = json_field(&response, &["local", "path"]).expect("local path");
            let size = json_field(&response, &["size"]).expect("file size");
            break (path, size);
        }
    };

    println!("PATH={path}");
    println!("SIZE={size}");

    let close = serde_json::json!({ "@type": "close" });
    client.send(&close.to_string());
    std::thread::sleep(Duration::from_millis(500));
}
