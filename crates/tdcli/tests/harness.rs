//! M.14.1 gate — scripted stdin → expected stderr fixtures, pinned against
//! the C++ `tg_cli` observable behavior (empirical runs against
//! `build-audit/tg_cli`, 2026-09-19):
//!
//! - every unknown non-empty command reports `Unknown command ... of length N`;
//! - empty lines log `CMD:[]` and don't report;
//! - control chars are removed before the split (a tab vanishes from the
//!   middle of a command), ANSI escapes are stripped;
//! - piped EOF does not exit the process (and a partial line at EOF is
//!   dropped — readline never delivers it);
//! - `q` logs `QUIT` and exits 0; lines queued behind `q` are dropped.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn normalize(stderr: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        // strip ANSI color codes (DefaultLog always colors, tty or not)
        let mut no_ansi = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                no_ansi.push(c);
            }
        }
        // normalize the environment-dependent brackets: `[unix.nanos]` → `[ts]`
        // and `[cli.rs:NN]` → `[cli.rs]`
        let mut s = String::new();
        let mut i = 0;
        while i < no_ansi.len() {
            if no_ansi.as_bytes()[i] == b'['
                && let Some(close) = no_ansi[i + 1..].find(']').map(|p| i + 1 + p)
            {
                let body = &no_ansi[i + 1..close];
                if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                    s.push_str("[ts]");
                    i = close + 1;
                    continue;
                }
                if body.starts_with("cli.rs:")
                    && body["cli.rs:".len()..].bytes().all(|b| b.is_ascii_digit())
                {
                    s.push_str("[cli.rs]");
                    i = close + 1;
                    continue;
                }
            }
            let ch = no_ansi[i..].chars().next().unwrap();
            s.push(ch);
            i += ch.len_utf8();
        }
        out.push(s);
    }
    // The eager-Td startup plane (the Track J eager `td_create_client_id`
    // fix) races the command processing, and the auto-config answers'
    // `system_version` follows the host — filter those blocks out instead
    // of baking the racy transcript into the goldens. The S81 bindings
    // smoke covers the startup update itself.
    let mut filtered: Vec<String> = Vec::new();
    let mut pending_header: Option<String> = None;
    let mut skip_lines = 0usize;
    let mut in_failed_send = false;
    for line in &out {
        if in_failed_send {
            // inside a `Failed to send: ...` request dump; the block ends
            // at the bare `}` that closes the request object
            if line == "}" {
                in_failed_send = false;
            }
            continue;
        }
        if line.contains("Failed to send: ") {
            in_failed_send = true;
            continue;
        }
        if skip_lines > 0 {
            skip_lines -= 1;
            continue;
        }
        if let Some(header) = pending_header.take() {
            if line.contains("authorizationStateWaitTdlibParameters")
                || line.contains("authorizationStateClosing")
            {
                // the whole 4-line update block (header, state line, the
                // two closers) is racy — drop it
                skip_lines = 2;
                continue;
            }
            filtered.push(header);
        }
        if line.contains("updateAuthorizationState {") {
            pending_header = Some(line.clone());
            continue;
        }
        filtered.push(line.clone());
    }
    filtered
}

struct Session {
    child: Child,
    tmpdir: std::path::PathBuf,
}

impl Session {
    fn spawn(input: &[u8]) -> Session {
        let tmpdir = std::env::temp_dir().join(format!(
            "tdcli-m14-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&tmpdir).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_td_cli"))
            .current_dir(&tmpdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        // stdin closes here — EOF right after the input
        Session { child, tmpdir }
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if start.elapsed() > timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_alive(&mut self) {
        assert!(
            self.wait_exit(Duration::from_millis(700)).is_none(),
            "the CLI must stay alive after piped EOF (C++ parity: it hangs)"
        );
    }

    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    fn stderr(&mut self) -> String {
        let mut out = String::new();
        use std::io::Read;
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        out
    }

    fn assert_exit_0_and_stderr(&mut self, expected: &[&str]) {
        let status = self
            .wait_exit(Duration::from_secs(10))
            .expect("process must exit after q");
        assert!(status.success(), "exit status: {status}");
        assert_eq!(normalize(&self.stderr()), expected);
        // the default second sink: tg_cli.log is created in the CWD
        assert!(self.tmpdir.join("tg_cli.log").exists());
    }
}

const CMD_HELLO: &str = "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[hello]";
const UNKNOWN_HELLO: &str =
    "[ 1][t 1][ts][cli.rs][!CliClient]\tUnknown command \"hello\" of length 5";

/// M.14.2: `init_td` creates the first client, and `q` tears it down — the
/// C++ `tg_cli` transcript for the same input carries the same lines (the
/// Closed update prints through the `TlStorerToString` debug format, then
/// the close accounting: `Td closed` → `Td with generation 1 is closed` →
/// `Ready to stop`).
const CREATING_TD: &str =
    "[ 2][t 1][ts][cli.rs][!CliClient]\tCreating new Td ClientActor1 with generation 1";

const QUIT_TAIL: &[&str] = &[
    "[ 2][t 1][ts][cli.rs][!CliClient]\tQUIT",
    "[ 2][t 1][ts][cli.rs][!CliClient]\tupdateAuthorizationState {",
    "  authorization_state = authorizationStateClosed {",
    "  }",
    "}",
    "[ 2][t 1][ts][cli.rs][!CliClient]\tTd closed",
    "[ 2][t 1][ts][cli.rs][!CliClient]\tTd with generation 1 is closed",
    "[ 2][t 1][ts][cli.rs][!CliClient]\tReady to stop",
];

#[test]
fn echo_unknown_and_quit() {
    let mut expected: Vec<&str> = vec![
        CREATING_TD,
        CMD_HELLO,
        UNKNOWN_HELLO,
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[foo bar]",
        "[ 1][t 1][ts][cli.rs][!CliClient]\tUnknown command \"foo bar\" of length 7",
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[q]",
    ];
    expected.extend_from_slice(QUIT_TAIL);
    let mut session = Session::spawn(b"hello\nfoo bar\nq\n");
    session.assert_exit_0_and_stderr(&expected);
}

#[test]
fn empty_line_and_leading_space() {
    let mut expected: Vec<&str> = vec![
        CREATING_TD,
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[]",
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[  leading space]",
        "[ 1][t 1][ts][cli.rs][!CliClient]\tUnknown command \"  leading space\" of length 15",
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[q]",
    ];
    expected.extend_from_slice(QUIT_TAIL);
    let mut session = Session::spawn(b"\n  leading space\nq\n");
    session.assert_exit_0_and_stderr(&expected);
}

#[test]
fn ansi_and_control_chars() {
    let mut expected: Vec<&str> = vec![
        CREATING_TD,
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[  tab   x]",
        "[ 1][t 1][ts][cli.rs][!CliClient]\tUnknown command \"  tab   x\" of length 9",
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[ansi]",
        "[ 1][t 1][ts][cli.rs][!CliClient]\tUnknown command \"ansi\" of length 4",
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[q]",
    ];
    expected.extend_from_slice(QUIT_TAIL);
    let mut session = Session::spawn(b"\t  tab   x\n\x1b[1Aansi\nq\n");
    session.assert_exit_0_and_stderr(&expected);
}

#[test]
fn eof_does_not_exit() {
    let mut session = Session::spawn(b"hello\n");
    session.assert_alive();
    // stderr can only be drained after kill (the live child holds the pipe)
    session.kill();
    assert_eq!(
        normalize(&session.stderr()),
        vec![CREATING_TD, CMD_HELLO, UNKNOWN_HELLO]
    );
}

#[test]
fn partial_line_at_eof_is_dropped() {
    let mut session = Session::spawn(b"abc");
    session.assert_alive();
    session.kill();
    assert_eq!(normalize(&session.stderr()), vec![CREATING_TD]);
}

#[test]
fn quit_drops_the_queued_tail() {
    let mut expected: Vec<&str> = vec![
        CREATING_TD,
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[x]",
        "[ 1][t 1][ts][cli.rs][!CliClient]\tUnknown command \"x\" of length 1",
        "[ 3][t 1][ts][cli.rs][!CliClient]\tCMD:[q]",
    ];
    expected.extend_from_slice(QUIT_TAIL);
    let mut session = Session::spawn(b"x\nq\ny\n");
    session.assert_exit_0_and_stderr(&expected);
}
