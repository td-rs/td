// The S68 oracle, Rust side: the same request corpus as
// `tdjson_oracle/cpp/dump_tdjson.cpp`, executed through the tdclient layer
// (the same code the tdjson cdylib exports). `verify.sh` diffs this output
// against the C++ dump.

use tdclient::client_json;

fn print(label: &str, response: Option<&str>) {
    match response {
        None => println!("[{label}] NULL"),
        Some(response) => println!("[{label}] {response}"),
    }
}

fn main() {
    // 1. The synchronous execute corpus.
    let exec_requests = [
        r#"{"@type":"getOption","name":"version"}"#,
        r#"{"@type":"getOption","name":"version","@extra":{"n":2}}"#,
        r#"{"@type":"getOption","name":"database_directory"}"#,
        r#"{"@type":"getMe"}"#,
        "{",
        "[1,2]",
        r#"{"@type":"noSuchType"}"#,
        r#"{"@type":"getJsonValue","json":"{\"a\":[1,true,null,\"x\"],\"b\":{\"c\":2.5}}"}"#,
        r#"{"@type":"getJsonString","json_value":{"@type":"jsonValueString","value":"hi"}}"#,
        r#"{"@type":"getJsonString","json_value":{"@type":"jsonValueObject","members":[{"@type":"jsonObjectMember","key":"k","value":{"@type":"jsonValueNumber","value":1.5}}]}}"#,
        r#"{"@type":"setLogVerbosityLevel","new_verbosity_level":2}"#,
        r#"{"@type":"getLogVerbosityLevel"}"#,
        r#"{"@type":"setLogVerbosityLevel","new_verbosity_level":10000}"#,
        r#"{"@type":"getLogTags"}"#,
        r#"{"@type":"setLogTagVerbosityLevel","tag":"no_such_tag","new_verbosity_level":3}"#,
        r#"{"@type":"setLogTagVerbosityLevel","tag":"","new_verbosity_level":3}"#,
        r#"{"@type":"getLogTagVerbosityLevel","tag":"mtproto"}"#,
        r#"{"@type":"setLogTagVerbosityLevel","tag":"mtproto","new_verbosity_level":4}"#,
        r#"{"@type":"getLogTagVerbosityLevel","tag":"mtproto"}"#,
        r#"{"@type":"testReturnError","error":{"@type":"error","code":418,"message":"teapot"}}"#,
        r#"{"@type":"setLogStream","log_stream":{"@type":"logStreamEmpty"}}"#,
        r#"{"@type":"getLogStream"}"#,
        r#"{"@type":"setLogStream","log_stream":{"@type":"logStreamDefault"}}"#,
        r#"{"@type":"getLogStream"}"#,
        r#"{"@type":"setLogStream","log_stream":{"@type":"logStreamFile","path":"","max_file_size":1024,"redirect_stderr":true}}"#,
    ];
    for request in exec_requests {
        print("execute", Some(&client_json::execute(request)));
    }

    // 2. The legacy client round-trip. The C++ side emits its startup
    // updates first (Td's get_common_state + the initial authorization
    // state); verify.sh filters them until the Rust Td actor (S70) does the
    // same. Receive until both responses arrive, then probe once more.
    {
        let client = client_json::ClientJson::new();
        client.send(r#"{"@type":"getOption","name":"version","@extra":[1,2]}"#);
        client.send(r#"{"@type":"getOption","name":"version"}"#);
        let mut got = 0;
        for _ in 0..10 {
            if got >= 2 {
                break;
            }
            let response = client.receive(1.0);
            // Prefix match: the response objects start with the @type,
            // while the updateOption startup updates only embed it.
            let is_answer = response
                .as_deref()
                .is_some_and(|r| r.starts_with(r#"{"@type":"optionValueString""#));
            print("legacy_receive", response.as_deref());
            if is_answer {
                got += 1;
            }
        }
        print("legacy_receive", client.receive(1.0).as_deref());
    }

    // 3. The new client_id API.
    {
        let client_id = client_json::create_client_id();
        client_json::send(client_id, r#"{"@type":"getOption","name":"version"}"#);
        let mut got = 0;
        for _ in 0..10 {
            if got >= 1 {
                break;
            }
            let response = client_json::receive(1.0);
            let is_answer = response
                .as_deref()
                .is_some_and(|r| r.starts_with(r#"{"@type":"optionValueString""#));
            print("new_receive", response.as_deref());
            if is_answer {
                got += 1;
            }
        }
        print("new_receive", client_json::receive(1.0).as_deref());
    }

    // 4. The new execute.
    print(
        "td_execute",
        Some(&client_json::execute(
            r#"{"@type":"getOption","name":"version"}"#,
        )),
    );
}
