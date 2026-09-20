//! M.14.1 — the `CliClient` harness (`td/telegram/cli.cpp` `class CliClient`):
//! stdin line pump, command queue, the `on_cmd` dispatch frame (the 19
//! sequential op-blocks with the C++ unknown-command counting), quit/close
//! lifecycle, and the `instance_` equivalent.
//!
//! Transport: the C++ CLI drives `ClientManager` through `ClientActor` inside
//! its scheduler (not the legacy `td_c_client` wrapper) — so the Rust CLI
//! sits on the same layer, the S70 `ClientManager`; the client wiring lands
//! in M.14.2 (`create_td`).
//!
//! Threading: the C++ actor-on-scheduler collapses to two OS threads — the
//! pump thread (the epoll stdin reader) and the main loop thread (the actor);
//! observable behavior (tick-drain order, EOF semantics) is ported, not the
//! epoll mechanics. (The plan sketches "on tokio"; the C++ CLI has no async
//! runtime, and the S70 ClientManager owns its scheduler threads — a plain
//! thread + condvar is the faithful mapping.)

use crate::tokenizer;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use tdutils::logging;

/// `cli.cpp:908` `const char *prompt = "td_cli> "` — printed on a tty only
/// (readline shows the prompt via `rl_set_prompt`; without readline nothing
/// is printed).
const PROMPT: &str = "td_cli> ";

pub struct CliClient {
    /// `std::queue<string> cmd_queue_`.
    cmd_queue: Mutex<VecDeque<String>>,
    /// The scheduler tick: the pump wakes the loop when a line arrives.
    condvar: Condvar,
    /// `bool close_flag_`.
    close_flag: AtomicBool,
    /// `bool ready_to_stop_`.
    ready_to_stop: AtomicBool,
    /// `bool is_stdin_reader_stopped_`.
    stdin_reader_stopped: AtomicBool,
    /// stdin(0) is a terminal — gates the prompt and the readline Ctrl-D
    /// "Closed" path.
    tty: bool,
}

/// `CliClient *CliClient::instance_` — the process-wide instance
/// (`quit_instance` target; the readline callbacks used it in C++).
static INSTANCE: OnceLock<CliClient> = OnceLock::new();

/// `CliClient::quit_instance`.
#[allow(dead_code)] // the C++ free `quit()` has no live caller either (the
// readline signal path used it); kept for parity
pub fn quit_instance() {
    instance().quit();
}

/// `CliClient::init`'s `instance_ = this`.
pub fn instance() -> &'static CliClient {
    INSTANCE.get_or_init(|| CliClient {
        cmd_queue: Mutex::new(VecDeque::new()),
        condvar: Condvar::new(),
        close_flag: AtomicBool::new(false),
        ready_to_stop: AtomicBool::new(false),
        stdin_reader_stopped: AtomicBool::new(false),
        tty: unsafe { libc::isatty(0) } == 1,
    })
}

impl CliClient {
    /// `CliClient::quit` (cli.cpp:2043): idempotent via `close_flag_`, logs
    /// `QUIT`, drops the client, unsubscribes stdin. The C++ `dump_memory_usage`
    /// (memprof) is not ported; `td_client_.reset()` arrives with M.14.2.
    fn quit(&self) {
        if self.close_flag.swap(true, Ordering::SeqCst) {
            return;
        }
        tdutils::log_warning!("QUIT");
        // td_client_.reset() — the ClientManager wiring lands in M.14.2.
        // Scheduler::unsubscribe(stdin_) →
        self.stdin_reader_stopped.store(true, Ordering::SeqCst);
        // M.14.1 interim: with no Td client (generation == closed_td == 0)
        // the C++ `on_closed` bookkeeping would set `ready_to_stop_`
        // immediately; M.14.2's `create_td`/`on_closed` replaces this with
        // the real generation counting.
        self.ready_to_stop.store(true, Ordering::SeqCst);
        self.condvar.notify_all();
    }

    /// `CliClient::add_cmd` (cli.cpp:9116).
    fn add_cmd(&self, cmd: String) {
        self.cmd_queue.lock().unwrap().push_back(cmd);
        self.condvar.notify_all();
    }

    /// `CliClient::on_cmd` (cli.cpp:3058) — the input preprocessing, the
    /// op/args split, and the 19 sequential op-blocks. The C++ dispatcher is
    /// a chain of independent `if (op == ...) ... else op_not_found_count++;`
    /// blocks: a command takes the arms of *every* matching block (no early
    /// return), and the unknown-command report fires only when all 19 are
    /// unmatched — hence `OP_BLOCK_COUNT = 19` (pinned empirically: every
    /// unknown command reports, cli.cpp:9020).
    fn on_cmd(&self, cmd: String) {
        let mut cmd = tokenizer::strip_ansi(cmd);
        cmd = tokenizer::remove_control_chars(cmd);
        tdutils::log_info!("CMD:[{cmd}]");

        let (op, args) = tokenizer::split(&cmd, ' ');

        const OP_BLOCK_COUNT: i32 = 19;
        let mut op_not_found_count = 0;

        // The 19 blocks, in cli.cpp order. M.14.1 implements only the final
        // block's `q`; the other blocks land as their slices do (M.14.3 auth,
        // M.14.4 messages/dialogs, M.14.5 files, M.14.6 misc/dev) — the
        // M.14.2 census fixture pins the per-block arm inventory.
        if !self.op_block_auth(&op, &args) {
            op_not_found_count += 1; // cli.cpp:3099 — the auth/account family (M.14.3)
        }
        if !self.op_block_recovery(&op, &args) {
            op_not_found_count += 1; // cli.cpp:3290 — recovery/imported contacts (M.14.3)
        }
        if !self.op_block_chats(&op, &args) {
            op_not_found_count += 1; // cli.cpp:3747 — chat list (M.14.4)
        }
        if !self.op_block_messages(&op, &args) {
            op_not_found_count += 1; // cli.cpp:4397 — send/edit/forward/... (M.14.4)
        }
        if !self.op_block_dialogs(&op, &args) {
            op_not_found_count += 1; // cli.cpp:4510 — dialog ops (M.14.4)
        }
        if !self.op_block_history(&op, &args) {
            op_not_found_count += 1; // cli.cpp:4821 — history/view/search (M.14.4)
        }
        if !self.op_block_files(&op, &args) {
            op_not_found_count += 1; // cli.cpp:5053 — download/upload/cancel (M.14.5)
        }
        if !self.op_block_stickers(&op, &args) {
            op_not_found_count += 1; // cli.cpp:5510 — sticker probes (M.14.6)
        }
        if !self.op_block_web_pages(&op, &args) {
            op_not_found_count += 1; // cli.cpp:5840 — webpage/rich-media probes (M.14.6)
        }
        if !self.op_block_misc(&op, &args) {
            op_not_found_count += 1; // cli.cpp:6061 — misc dev commands (M.14.6)
        }
        if !self.op_block_network(&op, &args) {
            op_not_found_count += 1; // cli.cpp:6840 — network/proxy probes (M.14.6)
        }
        if !self.op_block_logging(&op, &args) {
            op_not_found_count += 1; // cli.cpp:7362 — log/verbosity (M.14.6)
        }
        if !self.op_block_drafts(&op, &args) {
            op_not_found_count += 1; // cli.cpp:7554 — drafts/typing (M.14.4)
        }
        if !self.op_block_media(&op, &args) {
            op_not_found_count += 1; // cli.cpp:7714 — media/album extras (M.14.4)
        }
        if !self.op_block_dev_a(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8000 — dev probes A (M.14.6)
        }
        if !self.op_block_dev_b(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8200 — dev probes B (M.14.6)
        }
        if !self.op_block_dev_c(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8500 — dev probes C (M.14.6)
        }
        if !self.op_block_stats(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8850 — perf counters (M.14.6)
        }
        if !self.op_block_quit(&op, &args) {
            op_not_found_count += 1; // cli.cpp:8985 — q/dnq/alog/fatal/unreachable (M.14.6)
        }

        if op_not_found_count == OP_BLOCK_COUNT && !cmd.is_empty() {
            tdutils::log_error!("Unknown command \"{cmd}\" of length {}", cmd.len());
        }
    }

    // The block stubs return `false` (nothing matched yet) until their slice
    // lands; the last block carries `q`.

    fn op_block_auth(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_recovery(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_chats(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_messages(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_dialogs(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_history(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_files(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_stickers(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_web_pages(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_misc(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_network(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_logging(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_drafts(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_media(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_dev_a(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_dev_b(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_dev_c(&self, _op: &str, _args: &str) -> bool {
        false
    }

    fn op_block_stats(&self, _op: &str, _args: &str) -> bool {
        false
    }

    /// cli.cpp:8985 — `q` (the rest of the block — `dnq`, `alog`/`aloge`,
    /// `fatal`, `unreachable` — lands in M.14.6).
    fn op_block_quit(&self, op: &str, _args: &str) -> bool {
        if op == "q" {
            self.quit();
            true
        } else {
            false
        }
    }
}

/// `CliClient::loop` (cli.cpp:9027): init once, pump stdin lines through the
/// queue, drain the queue while not closing, exit when quit completes.
pub fn run(client: &'static CliClient) {
    // The C++ CliClient actor runs on scheduler thread 1 — its logs carry
    // `[t 1]`; the actor context supplies the `!CliClient` tag.
    logging::set_thread_id(1);
    logging::set_log_tag2("CliClient");
    if client.tty {
        eprint!("{PROMPT}");
    }

    spawn_pump(client);

    let mut is_inited = false;
    loop {
        if !is_inited {
            is_inited = true;
            // init() — create_td lands in M.14.2
        }

        while !client.close_flag.load(Ordering::SeqCst) {
            let cmd = client.cmd_queue.lock().unwrap().pop_front();
            match cmd {
                Some(cmd) => {
                    client.on_cmd(cmd);
                    if client.tty {
                        eprint!("{PROMPT}");
                    }
                }
                None => break,
            }
        }

        if client.ready_to_stop.load(Ordering::SeqCst)
            && client.close_flag.load(Ordering::SeqCst)
            && client.stdin_reader_stopped.load(Ordering::SeqCst)
        {
            break;
        }

        // Idle tick — wait for the pump.
        let mut queue = client.cmd_queue.lock().unwrap();
        while queue.is_empty() && !client.close_flag.load(Ordering::SeqCst) {
            queue = client.condvar.wait(queue).unwrap();
        }
    }
}

/// The stdin reader thread: the C++ epoll reader. Blocks until a line is
/// available, queues it, then drains whatever else is immediately readable
/// (the C++ `flush_read` + `while buffer non-empty` tick) so that piped
/// input is fully queued before processing — the observable drop-after-quit
/// ordering depends on it.
fn spawn_pump(client: &'static CliClient) {
    std::thread::Builder::new()
        .name("cli-stdin".to_string())
        .spawn(move || pump(client))
        .expect("failed to spawn the stdin pump thread");
}

fn pump(client: &'static CliClient) {
    let mut reader = StdinReader::new();
    'outer: loop {
        if client.close_flag.load(Ordering::SeqCst) {
            break;
        }
        // Blocking wait for data (the C++ idle tick on the pollable fd).
        if !reader.poll_blocking() {
            break;
        }
        loop {
            match reader.read_once() {
                Ok(true) => {}
                Ok(false) => {
                    // EOF. readline semantics: the Ctrl-D char on an empty
                    // line surfaces as the nullptr callback → FATAL "Closed"
                    // (tty only); a partial line is delivered first; piped
                    // EOF is silent — the process stays alive (pinned
                    // empirically against the C++ tg_cli, 2026-09-19).
                    if client.tty {
                        if !reader.has_pending() {
                            tdutils::log_fatal!("Closed");
                        }
                        if let Some(line) = reader.take_pending_line() {
                            if client.close_flag.load(Ordering::SeqCst) {
                                break 'outer;
                            }
                            client.add_cmd(line);
                            continue 'outer;
                        }
                    }
                    break 'outer;
                }
                Err(_) => break 'outer,
            }
            let mut delivered = false;
            while let Some(line) = reader.next_line() {
                delivered = true;
                if client.close_flag.load(Ordering::SeqCst) {
                    break 'outer;
                }
                client.add_cmd(line);
            }
            if reader.poll_zero() {
                // more input is immediately available — keep draining
                continue;
            }
            if !delivered {
                // a partial line is buffered — wait for the rest
                break;
            }
            break;
        }
    }
    client.stdin_reader_stopped.store(true, Ordering::SeqCst);
    client.condvar.notify_all();
}

/// A raw-fd(0) line reader with the C++ `BufferedStdin`-style semantics:
/// `poll` for readability, `read` only when readable, lines terminated by
/// `\n` (a trailing `\r` is stripped like `process_stdin`), partial lines
/// held until newline or EOF.
struct StdinReader {
    buffer: Vec<u8>,
    start: usize,
}

impl StdinReader {
    fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(4096),
            start: 0,
        }
    }

    /// `poll` with a blocking timeout; `false` on poll error (read would not
    /// help — treated like EOF by the caller).
    fn poll_blocking(&self) -> bool {
        poll_fd(0, -1)
    }

    /// `poll` with a zero timeout — is input available right now?
    fn poll_zero(&self) -> bool {
        poll_fd(0, 0)
    }

    /// Read once; `Ok(true)` — some bytes, `Ok(false)` — EOF.
    fn read_once(&mut self) -> std::io::Result<bool> {
        let mut tmp = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(0, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Ok(false);
            }
            self.buffer.extend_from_slice(&tmp[..n as usize]);
            return Ok(true);
        }
    }

    /// The next `\n`-terminated line (trailing `\r` stripped).
    fn next_line(&mut self) -> Option<String> {
        let pos = self.buffer[self.start..].iter().position(|&b| b == b'\n')?;
        let end = self.start + pos;
        let mut line = String::from_utf8_lossy(&self.buffer[self.start..end]).into_owned();
        self.start = end + 1;
        if self.start == self.buffer.len() {
            self.start = 0;
            self.buffer.clear();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        Some(line)
    }

    /// A partial (newline-less) line is buffered.
    fn has_pending(&self) -> bool {
        self.start < self.buffer.len()
    }

    /// The partial line, for the tty Ctrl-D delivery path.
    fn take_pending_line(&mut self) -> Option<String> {
        if !self.has_pending() {
            return None;
        }
        let line = String::from_utf8_lossy(&self.buffer[self.start..]).into_owned();
        self.start = 0;
        self.buffer.clear();
        Some(line)
    }
}

fn poll_fd(fd: libc::c_int, timeout_ms: libc::c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        if r == 0 {
            return false;
        }
        return pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::logging::LogInterface;

    struct Sink {
        lines: Mutex<Vec<(i32, String)>>,
    }

    impl LogInterface for Sink {
        fn do_append(&self, log_level: i32, slice: &str) {
            self.lines
                .lock()
                .unwrap()
                .push((log_level, slice.to_string()));
        }
    }

    static SINK: Sink = Sink {
        lines: Mutex::new(Vec::new()),
    };

    /// The on_cmd preprocessing + dispatch frame, pinned against the C++
    /// observable behavior (empirical tg_cli runs, 2026-09-19): every unknown
    /// non-empty command reports, empty commands don't.
    #[test]
    fn dispatch_frame_unknown_quirk() {
        let guard = TEST_LOCK.lock().unwrap();
        SINK.lines.lock().unwrap().clear();
        logging::set_log_interface(Some(&SINK));

        let client = test_client();
        client.add_cmd("hello".to_string());
        client.add_cmd(String::new());
        while let Some(cmd) = client.cmd_queue.lock().unwrap().pop_front() {
            if client.close_flag.load(Ordering::SeqCst) {
                break;
            }
            client.on_cmd(cmd);
        }
        logging::set_log_interface(None);

        let lines = SINK.lines.lock().unwrap();
        let mut out = Vec::new();
        for (level, line) in lines.iter() {
            if (*level == logging::INFO && line.contains("CMD:["))
                || (*level == logging::ERROR && line.contains("Unknown command"))
            {
                let body = line.rsplit('\t').next().unwrap();
                out.push(body.trim_end_matches('\n').to_string());
            }
        }
        assert_eq!(
            out,
            vec![
                "CMD:[hello]",
                "Unknown command \"hello\" of length 5",
                "CMD:[]",
            ]
        );
        drop(guard);
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_client() -> CliClient {
        CliClient {
            cmd_queue: Mutex::new(VecDeque::new()),
            condvar: Condvar::new(),
            close_flag: AtomicBool::new(false),
            ready_to_stop: AtomicBool::new(false),
            stdin_reader_stopped: AtomicBool::new(false),
            tty: false,
        }
    }
}
