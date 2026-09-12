//! Port of `td/utils/tests.h` + `td/utils/tests.cpp`: tdlib's mini test harness.
//!
//! Provides the runner with per-test timing, substring filters, offset and
//! stress mode, step-resumable tests, the `REGRESSION_VERIFY` context chain,
//! stage barriers, and the file-backed regression tester. Tests are registered
//! statically with [`crate::td_test!`] and run by binaries that call
//! [`TestsRunner::run_all`] on the [`default_runner`] — the same model as the
//! C++ `TEST()` macro plus a `run_all()` binary entry. Registration happens
//! before `main` via the platform's C constructor section, with no extra
//! dependencies.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Stand-in for `td::Status` until it is ported in S18.
pub type TestStatus = Result<(), String>;

/// A tdlib-style test: either a plain `run`, or a sequence of `step`s driven
/// by the runner (the default `step` runs the whole test at once).
///
/// `Send` because a step-resumable test may be resumed by whichever thread
/// holds the runner next.
pub trait Test: Send {
    fn run(&mut self) {
        while self.step() {}
    }

    fn step(&mut self) -> bool {
        self.run();
        false
    }
}

/// Context visible to a running test through [`with_context`] — the port of
/// `td::TestContext`. `name` returns an owned string like the C++
/// `virtual CSlice name()`.
pub trait TestContext {
    fn name(&self) -> String;

    fn verify(&self, data: &str) -> TestStatus;
}

thread_local! {
    static CONTEXT_STACK: RefCell<Vec<Arc<RunnerContext>>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` with the innermost [`TestContext`] of the current thread, if any.
pub fn with_context<R>(f: impl FnOnce(&dyn TestContext) -> R) -> Option<R> {
    // The stack only ever holds clones of the runner's `Arc`d context, so
    // the borrow ends before `f` runs — `f` may itself push or pop contexts.
    let context = CONTEXT_STACK.with(|stack| stack.borrow().last().cloned())?;
    Some(f(context.as_ref()))
}

struct ContextGuard {
    _context: Arc<RunnerContext>,
}

impl ContextGuard {
    fn new(context: Arc<RunnerContext>) -> Self {
        CONTEXT_STACK.with(|stack| stack.borrow_mut().push(context.clone()));
        Self { _context: context }
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        CONTEXT_STACK.with(|stack| stack.borrow_mut().pop());
    }
}

/// Port of `REGRESSION_VERIFY(data)`: verifies `data` against the regression
/// tester of the running test, panicking on mismatch (`.ensure()` semantics).
pub fn verify_regression(data: &str) {
    match with_context(|context| context.verify(data)) {
        Some(Ok(())) => {}
        Some(Err(err)) => panic!("{err}"),
        None => panic!("REGRESSION_VERIFY called outside of a running test"),
    }
}

/// Picks one of the weighted steps at random; `rnd` plays the role of the C++
/// `Random` template parameter and must return a `u32`.
pub struct RandomSteps {
    steps: Vec<Step>,
    steps_sum: u32,
}

pub struct Step {
    pub func: Box<dyn FnMut()>,
    pub weight: u32,
}

impl RandomSteps {
    pub fn new(steps: Vec<Step>) -> Self {
        let steps_sum = steps.iter().map(|step| step.weight).sum();
        Self { steps, steps_sum }
    }

    pub fn step<R: FnMut() -> u32>(&mut self, rnd: &mut R) {
        let mut w = rnd() % self.steps_sum;
        for step in &mut self.steps {
            if w < step.weight {
                (step.func)();
                return;
            }
            w -= step.weight;
        }
    }
}

/// Busy-waiting stage barrier (`td::StageWait`).
#[derive(Default)]
pub struct StageWait {
    value: AtomicU64,
}

impl StageWait {
    pub fn wait(&self, need: u64) {
        self.value.fetch_add(1, Ordering::Release);
        while self.value.load(Ordering::Acquire) < need {
            thread::sleep(Duration::from_micros(1));
        }
    }
}

/// Condition-variable stage barrier (`td::StageMutex`).
#[derive(Default)]
pub struct StageMutex {
    mutex: Mutex<u64>,
    cond: Condvar,
}

impl StageMutex {
    pub fn wait(&self, need: u64) {
        let mut value = self.mutex.lock().unwrap();
        *value += 1;
        if *value == need {
            self.cond.notify_all();
            return;
        }
        while *value < need {
            value = self.cond.wait(value).unwrap();
        }
    }
}

pub use StageMutex as Stage;

/// `td::rand_string`: `len` random bytes in `[from, to]`. The bytes are raw,
/// not UTF-8, matching the C++ `static_cast<char>(Random::fast(...))`.
/// `rnd(from, to)` must return a value in the inclusive range; the global-RNG
/// overload lands with the Random port in S21.
pub fn rand_string_with(
    rnd: &mut impl FnMut(i32, i32) -> i32,
    from: i32,
    to: i32,
    len: usize,
) -> Vec<u8> {
    (0..len).map(|_| rnd(from, to) as u8).collect()
}

/// `td::rand_split`: splits `data` into chunks of random length (1..=10 or
/// 100..=200 bytes, chosen by coin flip).
pub fn rand_split_with(rnd: &mut impl FnMut(i32, i32) -> i32, data: &[u8]) -> Vec<Vec<u8>> {
    let mut res = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let len = if rnd(0, 1) == 1 {
            rnd(1, 10) as usize
        } else {
            rnd(100, 200) as usize
        };
        res.push(data[pos..(pos + len).min(data.len())].to_vec());
        pos += len;
    }
    res
}

pub trait RegressionTester: Send {
    fn verify_test(&self, name: &str, result: &str) -> TestStatus;

    fn save_db(&self);
}

#[derive(Default)]
struct TestDbEntry {
    result_hash: String,
}

/// File-backed `td::RegressionTesterImpl`: a `"<name> <sha256-hex>"`-per-line
/// db behind the magic `abce`, plus a cache dir holding one file per distinct
/// result, named by its hash.
pub struct FileRegressionTester {
    db_path: PathBuf,
    db_cache_dir: PathBuf,
    is_dirty: Cell<bool>,
    tests: RefCell<BTreeMap<String, TestDbEntry>>,
}

impl FileRegressionTester {
    pub fn destroy(db_path: &Path) {
        let _ = std::fs::remove_file(db_path);
    }

    /// `db_cache_dir` may be empty, in which case `<db_stem>.cache` is used.
    pub fn create(db_path: PathBuf, db_cache_dir: PathBuf) -> Self {
        let db_cache_dir = if db_cache_dir.as_os_str().is_empty() {
            default_cache_dir(&db_path)
        } else {
            db_cache_dir
        };
        let this = Self {
            db_path,
            db_cache_dir,
            is_dirty: Cell::new(false),
            tests: RefCell::new(BTreeMap::new()),
        };
        let _ = this.load_db();
        std::fs::create_dir_all(&this.db_cache_dir).unwrap();
        this
    }

    fn load_db(&self) -> TestStatus {
        let data = std::fs::read_to_string(&self.db_path).map_err(|e| e.to_string())?;
        let mut words = data.split_whitespace();
        match words.next() {
            Some("abce") => {}
            other => return Err(format!("Wrong magic {other:?}")),
        }
        let mut tests = self.tests.borrow_mut();
        while let Some(name) = words.next() {
            let result_hash = words.next().unwrap_or("").to_string();
            tests.insert(name.to_string(), TestDbEntry { result_hash });
        }
        Ok(())
    }
}

impl RegressionTester for FileRegressionTester {
    fn verify_test(&self, name: &str, result: &str) -> TestStatus {
        let hash = sha256_hex(result.as_bytes());
        let mut tests = self.tests.borrow_mut();
        let entry = tests.entry(name.to_string()).or_default();
        if !entry.result_hash.is_empty() && entry.result_hash != hash {
            std::fs::write(self.db_cache_dir.join("WA"), result).unwrap();
            return Err(format!(
                "Test {} changed: expected = \"{}\" received = \"{}\"",
                name, entry.result_hash, hash
            ));
        }
        let result_cache_path = self.db_cache_dir.join(&hash);
        if !result_cache_path.exists() {
            std::fs::write(result_cache_path, result).unwrap();
        }
        if entry.result_hash.is_empty() {
            entry.result_hash = hash;
            self.is_dirty.set(true);
        }
        Ok(())
    }

    fn save_db(&self) {
        if !self.is_dirty.replace(false) {
            return;
        }
        let mut buf = String::from("abce\n");
        for (name, entry) in self.tests.borrow().iter() {
            writeln!(&mut buf, "{} {}", name, entry.result_hash).unwrap();
        }
        let mut new_db_path = self.db_path.clone().into_os_string();
        new_db_path.push(".new");
        let new_db_path = PathBuf::from(new_db_path);
        std::fs::write(&new_db_path, buf).unwrap();
        std::fs::rename(&new_db_path, &self.db_path).unwrap();
    }
}

fn default_cache_dir(db_path: &Path) -> PathBuf {
    let mut path = db_path.to_path_buf();
    path.set_extension("");
    let mut os = path.into_os_string();
    os.push(".cache");
    PathBuf::from(os)
}

type TestCreator = Box<dyn Fn() -> Box<dyn Test> + Send>;

struct TestEntry {
    creator: TestCreator,
    test: Option<Box<dyn Test>>,
}

struct State {
    it: usize,
    end: usize,
    is_running: bool,
    start: Instant,
}

impl State {
    fn new() -> Self {
        Self {
            it: 0,
            end: 0,
            is_running: false,
            start: Instant::now(),
        }
    }
}

/// The context-visible half of a running [`TestsRunner`]: the running
/// test's name and the regression tester. Held via `Arc` so the thread-local
/// context stack never borrows from the runner itself — the runner keeps
/// mutating through `&mut self` while a test runs, which would invalidate
/// any pointer derived from `&self` of the runner (Miri/stacked-borrows
/// rejects that, correctly). Interior state sits behind a `Mutex` (not
/// `RefCell`) so `TestsRunner` stays `Send` for the process-wide
/// [`default_runner`].
#[derive(Default)]
struct ContextState {
    name: String,
    regression_tester: Option<Box<dyn RegressionTester>>,
}

struct RunnerContext {
    state: Mutex<ContextState>,
}

impl RunnerContext {
    fn new() -> Self {
        Self {
            state: Mutex::new(ContextState::default()),
        }
    }

    fn set_name(&self, name: String) {
        self.state.lock().expect("context poisoned").name = name;
    }

    fn clear_name(&self) {
        self.state.lock().expect("context poisoned").name.clear();
    }

    fn set_regression_tester(&self, tester: Box<dyn RegressionTester>) {
        self.state.lock().expect("context poisoned").regression_tester = Some(tester);
    }

    fn save_db(&self) {
        let state = self.state.lock().expect("context poisoned");
        if let Some(tester) = state.regression_tester.as_ref() {
            tester.save_db();
        }
    }
}

/// Port of `td::TestsRunner`.
pub struct TestsRunner {
    stress_flag: bool,
    substr_filters: Vec<String>,
    offset: String,
    tests: Vec<(String, TestEntry)>,
    state: State,
    context: Arc<RunnerContext>,
}

impl TestsRunner {
    pub fn new() -> Self {
        Self {
            stress_flag: false,
            substr_filters: Vec::new(),
            offset: String::new(),
            tests: Vec::new(),
            state: State::new(),
            context: Arc::new(RunnerContext::new()),
        }
    }

    pub fn add_test<F>(&mut self, name: impl Into<String>, creator: F)
    where
        F: Fn() -> Box<dyn Test> + Send + 'static,
    {
        let name = name.into();
        if self.tests.iter().any(|(n, _)| *n == name) {
            panic!("Test name collision {name}");
        }
        self.tests.push((
            name,
            TestEntry {
                creator: Box::new(creator),
                test: None,
            },
        ));
    }

    /// Registers all registered test names, in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.tests.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// A leading `+`/`-` makes the filter inclusive/exclusive; anything else
    /// is an inclusive filter.
    pub fn add_substr_filter(&mut self, filter: String) {
        let filter = match filter.as_bytes().first() {
            Some(b'+') | Some(b'-') => filter,
            _ => format!("+{filter}"),
        };
        self.substr_filters.push(filter);
    }

    pub fn set_offset(&mut self, offset: String) {
        self.offset = offset;
    }

    pub fn set_stress_flag(&mut self, flag: bool) {
        self.stress_flag = flag;
    }

    pub fn set_regression_tester(&mut self, regression_tester: Box<dyn RegressionTester>) {
        self.context.set_regression_tester(regression_tester);
    }

    pub fn run_all(&mut self) {
        while self.run_all_step() {}
    }

    /// Advances the runner by one step (a whole test, or one `step` of a
    /// step-based test). Returns true while there is (potentially) more to
    /// run — forever, in stress mode.
    pub fn run_all_step(&mut self) -> bool {
        let _context_guard = ContextGuard::new(self.context.clone());
        if self.state.it == self.state.end {
            self.state.end = self.tests.len();
            self.state.it = 0;
        }

        let mut skip_tests = true;
        while self.state.it != self.state.end {
            let name = self.tests[self.state.it].0.clone();
            if !self.state.is_running {
                let mut ok = true;
                for filter in &self.substr_filters {
                    let is_match = name.contains(&filter[1..]);
                    if is_match != (filter.as_bytes()[0] == b'+') {
                        ok = false;
                        break;
                    }
                }
                if name.contains(&self.offset) {
                    skip_tests = false;
                }
                if !ok || skip_tests {
                    self.state.it += 1;
                    continue;
                }
                eprintln!("Run test name = \"{name}\"");
                self.state.start = Instant::now();
                self.state.is_running = true;
                self.context.set_name(name);

                debug_assert!(self.tests[self.state.it].1.test.is_none());
                let creator = &self.tests[self.state.it].1.creator;
                self.tests[self.state.it].1.test = Some(creator());
            }

            let more = self.tests[self.state.it].1.test.as_mut().unwrap().step();
            if more {
                break;
            }

            self.tests[self.state.it].1.test = None;

            let passed = self.state.start.elapsed();
            eprintln!("{}", as_time(passed.as_secs_f64()));
            self.context.save_db();
            self.context.clear_name();
            self.state.is_running = false;
            self.state.it += 1;
        }

        let ret = self.state.it != self.state.end;
        if !ret {
            self.state = State::new();
        }
        ret || self.stress_flag
    }
}

impl Default for TestsRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl TestContext for RunnerContext {
    fn name(&self) -> String {
        let state = self.state.lock().expect("context poisoned");
        assert!(
            !state.name.is_empty(),
            "TestsRunner::name() called while no test is running"
        );
        state.name.clone()
    }

    fn verify(&self, data: &str) -> TestStatus {
        let state = self.state.lock().expect("context poisoned");
        let Some(regression_tester) = state.regression_tester.as_ref() else {
            eprintln!("{data}");
            eprintln!(
                "Cannot verify and save <{}> answer. Use --regression <regression_db> option",
                state.name
            );
            return Ok(());
        };
        regression_tester.verify_test(&format!("{}_default", state.name), data)
    }
}

/// The process-wide runner that [`crate::td_test!`] registers into.
pub fn default_runner() -> &'static Mutex<TestsRunner> {
    static RUNNER: OnceLock<Mutex<TestsRunner>> = OnceLock::new();
    RUNNER.get_or_init(|| Mutex::new(TestsRunner::new()))
}

fn sha256_hex(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(digest.len() * 2);
    for &byte in &digest {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

/// Port of `td::as_time`: 1 fractional digit with an ns/us/ms/s unit chosen
/// so the value stays below 10 of the next-larger unit.
fn as_time(seconds: f64) -> String {
    const DURATIONS: [(&str, f64); 4] = [("ns", 1e-9), ("us", 1e-6), ("ms", 1e-3), ("s", 1.0)];
    let mut i = 0;
    while i + 1 < DURATIONS.len() && seconds > 10.0 * DURATIONS[i + 1].1 {
        i += 1;
    }
    format!("{:.1}{}", seconds / DURATIONS[i].1, DURATIONS[i].0)
}

/// Port of the `TEST(test_case_name, test_name)` macro: defines the test type
/// and registers it, before `main`, on the [`default_runner`] under
/// `"Test_<test_case_name>_<test_name>"`.
///
/// The C++ `Test_<case>_<name>` class maps to the nested module
/// `<case>::<name>::Test` — no identifier concatenation (and thus no
/// proc-macro helper crate) is needed; the registered name matches the C++
/// `TEST_NAME` stringification exactly.
///
/// ```
/// tdutils::td_test!(Http, host, {
///     // body of Test::run
/// });
/// ```
#[macro_export]
macro_rules! td_test {
    ($test_case:ident, $test_name:ident, $body:block) => {
        #[doc = concat!("tdlib test `Test_", stringify!($test_case), "_", stringify!($test_name), "`.")]
        #[allow(non_snake_case, non_camel_case_types)]
        pub mod $test_case {
            #[doc = concat!("tdlib test `Test_", stringify!($test_case), "_", stringify!($test_name), "`.")]
            #[allow(non_snake_case, non_camel_case_types)]
            pub mod $test_name {
                pub struct Test;

                impl $crate::tests::Test for Test {
                    fn run(&mut self) $body
                }

                #[doc(hidden)]
                #[allow(dead_code)] // referenced by the C runtime, not Rust
                #[used]
                // Register before `main` through the platform's C
                // constructor section — the same pre-main mechanism as the
                // C++ static registrar objects of td/utils/tests.h. The
                // entry must be a plain `extern "C" fn()` pointer: that is
                // the ABI loaders (and Miri's loader emulation) use to call
                // these entries. (`ctor` wrapped the body in an
                // `extern "C" fn() -> usize` shim, which Miri correctly
                // rejects as an ABI mismatch.) Targets without a constructor
                // section (e.g. wasm, until S82) simply don't register.
                #[cfg_attr(
                    any(
                        target_os = "linux",
                        target_os = "android",
                        target_os = "freebsd",
                        target_os = "netbsd",
                        target_os = "openbsd",
                        target_os = "dragonfly",
                        target_os = "illumos",
                        target_os = "solaris",
                        target_os = "haiku",
                        target_os = "emscripten",
                    ),
                    unsafe(link_section = ".init_array")
                )]
                #[cfg_attr(
                    target_vendor = "apple",
                    unsafe(link_section = "__DATA,__mod_init_func")
                )]
                #[cfg_attr(
                    all(target_os = "windows", target_env = "msvc"),
                    unsafe(link_section = ".CRT$XCU")
                )]
                #[cfg_attr(
                    all(target_os = "windows", target_env = "gnu"),
                    unsafe(link_section = ".ctors")
                )]
                static __REGISTER: extern "C" fn() = {
                    extern "C" fn register() {
                        $crate::tests::default_runner()
                            .lock()
                            .expect("default test runner poisoned")
                            .add_test(
                                concat!(
                                    "Test_",
                                    stringify!($test_case),
                                    "_",
                                    stringify!($test_name)
                                ),
                                || Box::new(Test),
                            );
                    }
                    register
                };
            }
        }
    };
}

#[cfg(test)]
mod smoke {
    use super::*;
    use std::sync::Arc;

    struct ProbeTest {
        name: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Test for ProbeTest {
        fn run(&mut self) {
            self.log.lock().unwrap().push(self.name);
        }
    }

    fn probe_runner(names: &[&'static str], log: &Arc<Mutex<Vec<&'static str>>>) -> TestsRunner {
        let mut runner = TestsRunner::new();
        for &name in names {
            let log = log.clone();
            runner.add_test(name, move || {
                Box::new(ProbeTest {
                    name,
                    log: log.clone(),
                })
            });
        }
        runner
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tdutils-tests-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn runs_tests_in_registration_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut runner = probe_runner(&["Test_A_b", "Test_A_c", "Test_B_a"], &log);
        runner.run_all();
        assert_eq!(*log.lock().unwrap(), ["Test_A_b", "Test_A_c", "Test_B_a"]);
    }

    #[test]
    fn substr_filter_includes_by_default_and_excludes_with_minus() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut runner = probe_runner(&["Test_Http_host", "Test_Http_url", "Test_Db_binlog"], &log);
        runner.add_substr_filter("Http".to_string());
        runner.run_all();
        assert_eq!(*log.lock().unwrap(), ["Test_Http_host", "Test_Http_url"]);

        let log = Arc::new(Mutex::new(Vec::new()));
        let mut runner = probe_runner(&["Test_Http_host", "Test_Http_url", "Test_Db_binlog"], &log);
        runner.add_substr_filter("+Http".to_string());
        runner.add_substr_filter("-url".to_string());
        runner.run_all();
        assert_eq!(*log.lock().unwrap(), ["Test_Http_host"]);
    }

    #[test]
    fn offset_skips_everything_before_the_match() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut runner = probe_runner(&["Test_A_b", "Test_A_c", "Test_B_a"], &log);
        runner.set_offset("Test_A_c".to_string());
        runner.run_all();
        assert_eq!(*log.lock().unwrap(), ["Test_A_c", "Test_B_a"]);
    }

    #[test]
    fn stress_flag_restarts_the_pass() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut runner = probe_runner(&["Test_A_b"], &log);
        runner.set_stress_flag(true);
        assert!(runner.run_all_step());
        assert!(runner.run_all_step());
        assert_eq!(*log.lock().unwrap(), ["Test_A_b", "Test_A_b"]);
    }

    #[test]
    fn step_test_resumes_across_run_all_step_calls() {
        struct StepProbeTest {
            remaining: usize,
            log: Arc<Mutex<Vec<&'static str>>>,
        }
        impl Test for StepProbeTest {
            fn step(&mut self) -> bool {
                self.log.lock().unwrap().push("step");
                self.remaining -= 1;
                self.remaining > 0
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let mut runner = TestsRunner::new();
        let log2 = log.clone();
        runner.add_test("Test_Step_resume", move || {
            Box::new(StepProbeTest {
                remaining: 3,
                log: log2.clone(),
            })
        });

        let mut results = Vec::new();
        loop {
            results.push(runner.run_all_step());
            if !results.last().copied().unwrap() {
                break;
            }
        }
        assert_eq!(results, [true, true, false]);
        assert_eq!(*log.lock().unwrap(), ["step", "step", "step"]);
    }

    #[test]
    #[should_panic(expected = "Test name collision")]
    fn duplicate_test_name_panics() {
        let mut runner = TestsRunner::new();
        runner.add_test("Test_A_b", || {
            Box::new(ProbeTest {
                name: "Test_A_b",
                log: Arc::new(Mutex::new(Vec::new())),
            })
        });
        runner.add_test("Test_A_b", || {
            Box::new(ProbeTest {
                name: "Test_A_b",
                log: Arc::new(Mutex::new(Vec::new())),
            })
        });
    }

    #[test]
    fn stage_mutex_releases_all_participants_at_once() {
        use std::sync::atomic::AtomicUsize;

        let stage = Arc::new(Stage::default());
        let passed = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let stage = stage.clone();
                let passed = passed.clone();
                thread::spawn(move || {
                    stage.wait(4);
                    passed.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();

        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            passed.load(Ordering::SeqCst),
            0,
            "nobody passes before all 4 arrive"
        );
        stage.wait(4);
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(passed.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn stage_wait_releases_all_participants_at_once() {
        let stage = Arc::new(StageWait::default());
        let stage2 = stage.clone();
        let handle = thread::spawn(move || stage2.wait(2));
        thread::sleep(Duration::from_millis(50));
        stage.wait(2);
        handle.join().unwrap();
    }

    #[test]
    fn random_steps_pick_weighted_step() {
        let picked = Arc::new(Mutex::new(Vec::new()));
        let mut steps = Vec::new();
        for (name, weight) in [("a", 1u32), ("b", 2), ("c", 3)] {
            let picked = picked.clone();
            steps.push(Step {
                func: Box::new(move || picked.lock().unwrap().push(name)),
                weight,
            });
        }
        let mut random_steps = RandomSteps::new(steps);

        let draws = [0u32, 2, 5];
        let mut queue = draws.to_vec();
        let mut rnd = || queue.remove(0);
        for _ in draws {
            random_steps.step(&mut rnd);
        }
        assert_eq!(*picked.lock().unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn rand_string_stays_in_range_and_rand_split_reassembles() {
        let mut counter = 0i32;
        let mut rnd = move |from: i32, to: i32| {
            counter += 1;
            from + (counter.rem_euclid(to - from + 1))
        };

        let s = rand_string_with(&mut rnd, 32, 126, 1000);
        assert_eq!(s.len(), 1000);
        assert!(s.iter().all(|&b| (32..=126).contains(&b)));

        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let chunks = rand_split_with(&mut rnd, &data);
        assert!(
            chunks
                .iter()
                .all(|c| (1..=10).contains(&c.len()) || (100..=200).contains(&c.len()))
        );
        assert!(chunks.iter().flatten().copied().eq(data));
    }

    #[test]
    fn regression_tester_save_and_reload_round_trip() {
        let dir = temp_dir("regression");
        let db_path = dir.join("test.regression");
        let expected_hash = sha256_hex(b"hello");

        let tester = FileRegressionTester::create(db_path.clone(), PathBuf::new());
        assert!(dir.join("test.cache").exists());
        tester.verify_test("Test_X_y", "hello").unwrap();
        tester.save_db();
        let db = std::fs::read_to_string(&db_path).unwrap();
        let expected = format!("abce\nTest_X_y {expected_hash}\n");
        assert_eq!(db, expected);

        let tester = FileRegressionTester::create(db_path.clone(), PathBuf::new());
        tester.verify_test("Test_X_y", "hello").unwrap();
        let err = tester.verify_test("Test_X_y", "world").unwrap_err();
        assert!(err.contains("changed"), "{err}");
        assert_eq!(Sha256::output_size(), expected_hash.len() / 2);
    }

    #[test]
    fn runner_verifies_through_the_regression_tester() {
        struct RegProbe {
            data: &'static str,
        }
        impl Test for RegProbe {
            fn run(&mut self) {
                verify_regression(self.data);
            }
        }

        let dir = temp_dir("runner-regression");
        let db_path = dir.join("test.regression");
        let mut runner = TestsRunner::new();
        runner.set_regression_tester(Box::new(FileRegressionTester::create(
            db_path.clone(),
            PathBuf::new(),
        )));
        runner.add_test("Test_Reg_smoke", || Box::new(RegProbe { data: "payload" }));
        runner.run_all();

        let db = std::fs::read_to_string(&db_path).unwrap();
        let expected = format!("abce\nTest_Reg_smoke_default {}\n", sha256_hex(b"payload"));
        assert_eq!(db, expected);
    }

    #[test]
    #[should_panic(expected = "changed")]
    fn regression_mismatch_panics_through_verify_regression() {
        struct RegProbe {
            data: &'static str,
        }
        impl Test for RegProbe {
            fn run(&mut self) {
                verify_regression(self.data);
            }
        }

        let dir = temp_dir("regression-mismatch");
        let db_path = dir.join("test.regression");
        let tester = FileRegressionTester::create(db_path.clone(), PathBuf::new());
        tester.verify_test("Test_Reg_panic_default", "old").unwrap();
        tester.save_db();

        let mut runner = TestsRunner::new();
        runner.set_regression_tester(Box::new(FileRegressionTester::create(
            db_path,
            PathBuf::new(),
        )));
        runner.add_test("Test_Reg_panic", || Box::new(RegProbe { data: "new" }));
        runner.run_all();
    }
}

#[cfg(test)]
mod ctor_smoke_tests {
    // Registered before main by the constructor-section entry generated in
    // `td_test!`.
    crate::td_test!(Smoke, ctor_registration, {});

    #[test]
    fn ctor_registered_test_is_in_default_runner() {
        let runner = crate::tests::default_runner().lock().unwrap();
        assert!(
            runner.names().contains(&"Test_Smoke_ctor_registration"),
            "registered names: {:?}",
            runner.names()
        );
    }
}
