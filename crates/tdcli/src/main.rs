//! `td/telegram/cli.cpp` `td::main` — the CLI entry point (`tg_cli`).
//!
//! M.14.1 ports the harness frame: signals, the log-message callback, the
//! log setup, and the run loop. The option parsing (`-v`, `-l`, `--test`,
//! `-W`, `-n`, `--api-id`, `--api-hash`) and the api-id/api-hash requirement
//! check land in M.14.6 with the argument family; `create_td` lands in M.14.2.

mod cli;
mod log;
mod tokenizer;

/// `on_log_message` (cli.cpp:9217): the C++ prints verbosity-0 (fatal)
/// messages to stderr as "Fatal error: ".
fn on_log_message(verbosity_level: i32, message: &str) {
    if verbosity_level == 0 {
        eprint!("Fatal error: {message}");
    }
}

fn main() {
    // ExitGuard/ThreadIdGuard: the C++ guards track memory usage (memprof —
    // not ported) and the scheduler thread ids; nothing to do here.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    // C++: set_signal_handler(SignalType::Error/Abort, fail_signal) — prints
    // the signal number and spins forever for the debugger. Not ported: Rust
    // aborts on SIGSEGV/SIGABRT (recorded divergence — see the README).
    tdutils::logging::set_log_message_callback(0, Some(on_log_message));
    // std::locale::global("C") — Rust formatting is locale-independent.

    log::setup();

    cli::run(cli::instance());

    // The C++ SCOPE_EXIT: `log_interface = &null_log`.
    tdutils::logging::set_log_interface(Some(tdutils::logging::null_log_interface()));
}
