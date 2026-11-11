//! The one way to read HTML: a linear scan that cuts the obvious hostile shapes, then the
//! parser in a worker process that dies at its limits, in a parse slot.
//!
//! A page parse is CPU work no timeout can stop, and some inputs make the parser superlinear
//! (one tag with tens of thousands of attributes, tens of thousands of unclosed elements, end
//! tags the parser ignores). Every HTML parse in the crate goes through `document` or `fragment`
//! (a test checks no other code names the parser). The bound does not depend on predicting what
//! the parser does with a text:
//!
//! 1. **The worker.** Parses run in a child process (`pinakesd parse-worker`, or the
//!    `pinakes-parse` binary) under `RLIMIT_CPU` (`PARSE_BUDGET` of CPU time) and `RLIMIT_AS`
//!    (`PARSE_MEMORY`). One `blocking` call is one job (`Job`): its parses share one worker,
//!    started at the first and ended with the job, so they share its CPU budget and one wall
//!    deadline, and a watchdog kills the worker at that deadline or as soon as the caller stops
//!    waiting. Past any limit the parse fails as `TooComplex` (the failure kind `too_complex`):
//!    no card. Every later parse of that job fails at once, so a loop over many fragments ends
//!    within the job's budget too. The worker answers with the tree, within `MAX_DEPTH` (a deeper
//!    node is left out) and `MAX_NODES` (more is `TooComplex`), so what the parent reads back is
//!    bounded and linear to rebuild. Text without a `<` holds no markup and is parsed in this
//!    process (in linear time), without a worker, after the same checks of the job: once the job
//!    is over, plain text is refused too. The daemon and the e2e stack are their own
//!    workers (`set_worker`); `PINAKES_PARSE_WORKER` names one for any other program. Without a
//!    worker (a library user, most tests) the parse runs in this process, fed a kibibyte at a
//!    time and stopped when the job has used its budget of the thread's CPU time, with the same
//!    caps on the tree.
//! 2. **The scan** reads the text once, the way the parser does, and cuts it at the first point
//!    that passes a cap, so the known hostile shapes stop at once and keep what precedes them:
//!
//!    | Cap | Real pages (149 pages of the hosts stable holds, October 2026) |
//!    | --- | --- |
//!    | `MAX_TAGS` 50,000 start tags | median 508, 95th percentile 2,805, most 16,440 (a Wikidata item) |
//!    | `MAX_DEPTH` 256 open elements | median 14, 95th percentile 31, most 62 |
//!    | `MAX_TAG_ATTRIBUTES` 64 per tag, end tags too | median 9, 95th percentile 14, most 29 |
//!    | `MAX_ATTRIBUTE_BYTES` 1 MiB per value | median 1 KB, 95th percentile 13 KB, most 121 KB |
//!    | `MAX_NAME_BYTES` 256 per tag name | most 27 (`markdown-accessiblity-table`) |
//!
//!    It follows the tokenizer (comments, `CDATA` in SVG and MathML, raw text, the script escape
//!    states, attributes on end tags) and the parts of tree building that decide what is markup
//!    and what is open (SVG and MathML content, integration points, end tags that stop at a
//!    special element or a scope boundary). Void elements and the ones a sibling closes (`p`,
//!    `li`, `td`, `option`, …) are not counted, so old markup with unclosed list items is not
//!    cut. The scan is a cheap early cut, not the guarantee: a shape it reads differently from
//!    the parser meets the worker's limits.
//!
//! A parse runs in `blocking`: on the blocking pool, holding one of a fixed number of permits
//! until the job ends (its worker dies within 50 ms of its caller giving up). In debug builds a
//! parse on a runtime thread outside `blocking` panics, so the tests find any path that skips it.
use anyhow::Context;
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, LocalName, Namespace, Prefix, QualName};
use scraper::node::{Comment, Doctype, Element, ProcessingInstruction, Text};
use scraper::Node;
use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

pub const MAX_TAGS: usize = 50_000;
pub const MAX_DEPTH: usize = 256;
pub const MAX_TAG_ATTRIBUTES: usize = 64;
pub const MAX_ATTRIBUTE_BYTES: usize = 1024 * 1024;
pub const MAX_NAME_BYTES: usize = 256;
/// The most nodes a parsed tree may hold (elements, text, comments). Of 149 real pages, the most
/// has 37,186 (a Wikidata item).
pub const MAX_NODES: usize = 500_000;
/// The most ancestors a walk up a parsed tree looks at: the depth cap and the elements it does
/// not count.
pub const MAX_ANCESTORS: usize = 2 * MAX_DEPTH;
/// The CPU time one parse may take, ten times more in a debug build's slower code. The largest
/// of the 149 real pages parses in 0.5 s in a debug build.
pub const PARSE_BUDGET: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(10)
} else {
    Duration::from_secs(1)
};
/// The address space a parse worker may use. A 4 MiB page of nothing but `<a>` tags, the
/// densest tree a page within the head budget can make, takes 600 MB in a debug build.
pub const PARSE_MEMORY: u64 = 1024 * 1024 * 1024;
/// The parser reads this much between two looks at the budget, without a worker.
const PARSE_CHUNK: usize = 1024;
/// The largest answer a worker may give (the tree, serialized): a tree within `MAX_NODES` from
/// a page within the head budget is far smaller.
const MAX_TREE_BYTES: usize = 64 * 1024 * 1024;

const VOID: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "image", "img",
    "input", "keygen", "link", "meta", "param", "source", "track", "wbr",
];
/// Elements a sibling or the parser closes implicitly: never counted toward depth.
const IMPLIED: &[&str] = &[
    "body", "caption", "colgroup", "dd", "dt", "head", "html", "li", "optgroup", "option", "p",
    "rb", "rp", "rt", "rtc", "tbody", "td", "tfoot", "th", "thead", "tr",
];
/// HTML elements whose content is text up to their own end tag (`script` has its own states).
const RAW: &[&str] = &[
    "iframe", "noembed", "noframes", "noscript", "script", "style", "textarea", "title", "xmp",
];
/// Start tags that end SVG or MathML content (`font` only with `color`, `face` or `size`).
const BREAKOUT: &[&str] = &[
    "b",
    "big",
    "blockquote",
    "body",
    "br",
    "center",
    "code",
    "dd",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "hr",
    "i",
    "img",
    "li",
    "listing",
    "menu",
    "meta",
    "nobr",
    "ol",
    "p",
    "pre",
    "ruby",
    "s",
    "small",
    "span",
    "strong",
    "strike",
    "sub",
    "sup",
    "table",
    "tt",
    "u",
    "ul",
    "var",
];
/// HTML elements an end tag of another name does not close past (the parser's "special" ones).
const SPECIAL: &[&str] = &[
    "address",
    "applet",
    "area",
    "article",
    "aside",
    "base",
    "basefont",
    "bgsound",
    "blockquote",
    "body",
    "br",
    "button",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dir",
    "div",
    "dl",
    "dt",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "iframe",
    "img",
    "input",
    "keygen",
    "li",
    "link",
    "listing",
    "main",
    "marquee",
    "menu",
    "meta",
    "nav",
    "noembed",
    "noframes",
    "noscript",
    "object",
    "ol",
    "p",
    "param",
    "plaintext",
    "pre",
    "script",
    "search",
    "section",
    "select",
    "source",
    "style",
    "summary",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
    "wbr",
    "xmp",
];
/// HTML elements an end tag of their own name closes when one is open above the nearest
/// `SCOPE` element ("in scope"), closing what is open inside it.
const SCOPED_END: &[&str] = &[
    "address",
    "applet",
    "article",
    "aside",
    "blockquote",
    "button",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "listing",
    "main",
    "marquee",
    "menu",
    "nav",
    "object",
    "ol",
    "pre",
    "search",
    "section",
    "select",
    "summary",
    "table",
    "template",
    "ul",
];
/// The elements that bound a scope (with the SVG and MathML integration points).
const SCOPE: &[&str] = &[
    "applet", "caption", "html", "marquee", "object", "table", "td", "template", "th",
];
/// Formatting elements: an end tag closes the nearest one in scope, whatever is open inside.
const FORMATTING: &[&str] = &[
    "a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small", "strike", "strong", "tt", "u",
];

/// Page parses that may run at once: the machine's parallelism, at least two.
static PERMITS: LazyLock<usize> = LazyLock::new(|| {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2)
});
static PARSES: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(*PERMITS)));
thread_local! {
    /// The parse job this thread runs, while it runs one (`blocking`, `off_runtime`).
    static JOB: RefCell<Option<Job>> = const { RefCell::new(None) };
}

/// The page parses running now.
pub fn running() -> usize {
    *PERMITS - PARSES.available_permits()
}

/// Sets its flag when dropped: the caller of `blocking` stopped waiting.
struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Run `parse` on the blocking pool once a parse permit is free, as one parse job: its parses
/// share one worker and one budget (`Job`). The permit is held until `parse` returns, even when
/// the caller has stopped waiting; from then on its parses fail at once and its worker is killed.
pub async fn blocking<T, F>(parse: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = permit().await?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = Cancel(cancelled.clone());
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        in_job(Job::new(PARSE_BUDGET, cancelled), parse)
    })
    .await?)
}

/// A parse permit, waited for on the runtime.
#[cfg(not(test))]
async fn permit() -> anyhow::Result<tokio::sync::OwnedSemaphorePermit> {
    Ok(PARSES.clone().acquire_owned().await?)
}
/// A parse permit. This crate's tests run on paused clocks, which tokio moves on whenever a
/// test's runtime has nothing to do, and every test in the process shares the permits: a wait on
/// the runtime for a permit another test holds would pass the test's timeouts at once (a
/// recorded fetch "exceeded 10 seconds" in no time, a card part unread). The wait runs on the
/// blocking pool instead, which keeps a paused clock where it is.
#[cfg(test)]
async fn permit() -> anyhow::Result<tokio::sync::OwnedSemaphorePermit> {
    if let Ok(permit) = PARSES.clone().try_acquire_owned() {
        return Ok(permit);
    }
    let runtime = tokio::runtime::Handle::current();
    Ok(
        tokio::task::spawn_blocking(move || runtime.block_on(PARSES.clone().acquire_owned()))
            .await??,
    )
}

/// Run a parse on a thread of its own, outside the runtime and its permits, as one parse job,
/// and wait for it: for tests and tools that parse from async code. Daemon code uses `blocking`;
/// a test checks no code in `src` calls this.
#[doc(hidden)]
pub fn off_runtime<T: Send>(parse: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| in_job(Job::new(PARSE_BUDGET, Arc::default()), parse))
            .join()
            .expect("parse thread")
    })
}

/// Run `f` with `job` as this thread's parse job, and end the job (and its worker) after.
fn in_job<T>(job: Job, f: impl FnOnce() -> T) -> T {
    struct End;
    impl Drop for End {
        fn drop(&mut self) {
            if let Some(job) = JOB.with(|j| j.borrow_mut().take()) {
                job.end();
            }
        }
    }
    JOB.with(|j| *j.borrow_mut() = Some(job));
    let _end = End;
    f()
}

/// Whether this thread's parse job is over (its caller gone, its deadline passed, its worker
/// dead): work in the job besides parsing (a feed's prefix) checks it and stops. False outside a
/// job.
pub fn job_over() -> bool {
    JOB.with(|j| {
        j.borrow().as_ref().is_some_and(|job| {
            job.over.is_some()
                || job.cancelled.load(Ordering::SeqCst)
                || Instant::now() >= job.deadline
        })
    })
}

fn check_slot() {
    debug_assert!(
        JOB.with(|j| j.borrow().is_some()) || tokio::runtime::Handle::try_current().is_err(),
        "an HTML parse on a runtime thread outside resolver::html::blocking"
    );
}

/// A parse that did not end within its limits: the page gets no card (`too_complex`).
#[derive(Debug)]
pub struct TooComplex {
    pub reason: &'static str,
}
impl std::fmt::Display for TooComplex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the page is too complex to parse ({})", self.reason)
    }
}
impl std::error::Error for TooComplex {}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Document = 0,
    Fragment = 1,
}

/// `text` parsed as an HTML document, within the caps and the job's limits.
pub fn document(text: &str) -> anyhow::Result<scraper::Html> {
    check_slot();
    parse(Kind::Document, bounded(text), PARSE_BUDGET)
}

/// `text` parsed as an HTML fragment, within the caps and the job's limits.
pub fn fragment(text: &str) -> anyhow::Result<scraper::Html> {
    check_slot();
    parse(Kind::Fragment, bounded(text), PARSE_BUDGET)
}

/// `text` parsed as a document with the scan's cut skipped: for the tests of the limits
/// themselves. Outside a job, it runs as a job of its own with `budget`.
#[doc(hidden)]
pub fn document_unscanned(text: &str, budget: Duration) -> anyhow::Result<scraper::Html> {
    check_slot();
    parse(Kind::Document, text, budget)
}

/// `text` up to the first cap it passes (see the module).
pub fn bounded(text: &str) -> &str {
    let mut scan = Scan::default();
    scan.feed(text.as_bytes());
    // A cut is at a `<`, a character boundary.
    scan.cut.map_or(text, |at| &text[..at])
}

/// Parse in this thread's job, or in a job of its own with `budget` outside one.
fn parse(kind: Kind, text: &str, budget: Duration) -> anyhow::Result<scraper::Html> {
    if JOB.with(|j| j.borrow().is_some()) {
        return JOB.with(|j| j.borrow_mut().as_mut().expect("a job").parse(kind, text));
    }
    let mut job = Job::new(budget, Arc::default());
    let result = job.parse(kind, text);
    job.end();
    result
}

/// The program that parses for this process.
#[derive(Debug)]
struct Worker {
    program: PathBuf,
    args: Vec<OsString>,
}
static WORKER: OnceLock<Option<Worker>> = OnceLock::new();

/// Parse in a child process, `program args… <cpu seconds>`, which runs `worker_main` (the daemon
/// itself with `parse-worker`). Set once, before any parse; a later call changes nothing.
pub fn set_worker(
    program: impl Into<PathBuf>,
    args: impl IntoIterator<Item = impl Into<OsString>>,
) {
    let _ = WORKER.set(Some(Worker {
        program: program.into(),
        args: args.into_iter().map(Into::into).collect(),
    }));
}

/// The worker, when one is set, or when `PINAKES_PARSE_WORKER` names a program that takes the
/// CPU seconds as its one argument (`pinakes-parse`).
fn worker() -> Option<&'static Worker> {
    WORKER
        .get_or_init(|| {
            Some(Worker {
                program: std::env::var_os("PINAKES_PARSE_WORKER")?.into(),
                args: vec![],
            })
        })
        .as_ref()
}

/// Whether parses run in a worker process.
pub fn has_worker() -> bool {
    worker().is_some()
}

/// Parse a small document through the worker: a daemon checks it can before it serves.
pub fn check_worker() -> anyhow::Result<()> {
    anyhow::ensure!(has_worker(), "no parse worker is set");
    let mut job = Job::new(PARSE_BUDGET, Arc::default());
    let html = job.parse(Kind::Document, "<p>worker</p>");
    let spawned = job.process.is_some();
    job.end();
    let html = html?;
    anyhow::ensure!(spawned, "the parse ran without its worker");
    let p = scraper::Selector::parse("p").expect("a selector");
    anyhow::ensure!(
        html.select(&p).next().map(|e| e.inner_html()).as_deref() == Some("worker"),
        "the parse worker answered a different tree"
    );
    Ok(())
}

/// One parse job: everything one `blocking` call (or one parse outside any) parses. Its parses
/// share one worker process, started at the first and ended with the job, so they share the
/// worker's CPU budget (`RLIMIT_CPU` counts the process's whole life) and one wall deadline.
/// Once the worker has died at a limit, or the caller has stopped waiting, every later parse of
/// the job fails at once: a loop over twenty thousand fragments ends in its budget, not in
/// twenty thousand of them. Without a worker, the job's parses share the thread's CPU budget.
struct Job {
    budget: Duration,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    /// The thread's CPU time when the job started (for parses without a worker).
    cpu_started: Duration,
    /// Why the job parses no more.
    over: Option<&'static str>,
    /// The worker did not start: the job's parses fail without trying again.
    start_failed: bool,
    /// A tree past `MAX_NODES` has been logged (once per job).
    node_cap_logged: bool,
    process: Option<Process>,
}

/// A running worker: its pipes, and the watchdog that kills it.
struct Process {
    shared: Arc<(Mutex<Option<std::process::Child>>, Condvar)>,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    watchdog: std::thread::JoinHandle<Option<&'static str>>,
}

impl Job {
    fn new(budget: Duration, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            budget,
            // A host under load runs the worker slower than its CPU time.
            deadline: Instant::now() + budget * 3 + Duration::from_secs(1),
            cancelled,
            cpu_started: thread_cpu(),
            over: None,
            start_failed: false,
            node_cap_logged: false,
            process: None,
        }
    }

    /// Every parse of the job, plain text too, passes the job's checks first: a job that is over
    /// (its caller gone, its deadline or its budget passed, its worker dead) parses nothing more.
    fn parse(&mut self, kind: Kind, text: &str) -> anyhow::Result<scraper::Html> {
        self.parse_with(kind, text, worker())
    }

    /// `parse`, with `worker` as the program (`None`: parse in this process).
    fn parse_with(
        &mut self,
        kind: Kind,
        text: &str,
        worker: Option<&Worker>,
    ) -> anyhow::Result<scraper::Html> {
        if let Some(reason) = self.over {
            return Err(TooComplex { reason }.into());
        }
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(self.end_with("the caller stopped waiting", text));
        }
        if Instant::now() >= self.deadline {
            return Err(self.end_with("the wall deadline", text));
        }
        // Text without a `<` holds no markup: the tokenizer reads it in linear time and there is
        // no tree to build, so it is parsed here (a calendar's plain descriptions). It counts
        // against the job's budget of this thread's CPU time, as every parse without a worker
        // does; a worker's parses count against the worker's own.
        let plain = !text.contains('<');
        let worker = if plain { None } else { worker };
        let Some(worker) = worker else {
            let left = self
                .budget
                .saturating_sub(thread_cpu().saturating_sub(self.cpu_started));
            if left.is_zero() {
                return Err(self.end_with("the CPU budget", text));
            }
            return parse_in_process(kind, text, left).map_err(|error| {
                if error.is::<TooComplex>() {
                    self.node_cap(text)
                } else {
                    error
                }
            });
        };
        if self.start_failed {
            anyhow::bail!("the parse worker did not start");
        }
        if self.process.is_none() {
            match self.start(worker) {
                Ok(process) => self.process = Some(process),
                Err(error) => {
                    // Once per job: a program that does not start will not on the next fragment.
                    self.start_failed = true;
                    return Err(error);
                }
            }
        }
        match self.ask(kind, text) {
            Ok(Some(html)) => Ok(html),
            // One tree refused; the worker goes on with the job's next parse.
            Ok(None) => Err(self.node_cap(text)),
            Err(_) => {
                // The worker is gone: its status, or the watchdog, says why.
                let reason = self.stop();
                Err(self.end_with(reason, text))
            }
        }
    }

    /// A tree refused at `MAX_NODES`: the job goes on, and the first such refusal of the job is
    /// logged, as the job's end is.
    fn node_cap(&mut self, text: &str) -> anyhow::Error {
        if !self.node_cap_logged {
            self.node_cap_logged = true;
            tracing::warn!(
                reason = "more nodes than the cap",
                bytes = text.len(),
                "an HTML parse did not end within its limits; the page gets no card"
            );
        }
        TooComplex {
            reason: "more nodes than the cap",
        }
        .into()
    }

    /// End the job's parsing for good, logged once, and the error for the parse that found it.
    fn end_with(&mut self, reason: &'static str, text: &str) -> anyhow::Error {
        self.over = Some(reason);
        tracing::warn!(
            reason,
            bytes = text.len(),
            "an HTML parse did not end within its limits; the page gets no card"
        );
        TooComplex { reason }.into()
    }

    fn start(&self, worker: &Worker) -> anyhow::Result<Process> {
        let mut child = Command::new(&worker.program)
            .args(&worker.args)
            .arg(self.budget.as_secs().max(1).to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!(
                    "the parse worker {} did not start",
                    worker.program.display()
                )
            })?;
        let stdin = child.stdin.take().expect("a piped stdin");
        let stdout = child.stdout.take().expect("a piped stdout");
        let shared = Arc::new((Mutex::new(Some(child)), Condvar::new()));
        // The child dies by itself at its CPU or memory limit; the watchdog kills one still there
        // at the job's wall deadline, or as soon as the caller stops waiting.
        let watchdog = {
            let (shared, cancelled, deadline) =
                (shared.clone(), self.cancelled.clone(), self.deadline);
            std::thread::spawn(move || {
                let (child, done) = &*shared;
                let mut slot = child.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    let running = slot.as_mut()?;
                    let reason = if cancelled.load(Ordering::SeqCst) {
                        Some("the caller stopped waiting")
                    } else if Instant::now() >= deadline {
                        Some("the wall deadline")
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        let _ = running.kill();
                        return Some(reason);
                    }
                    let wait = deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50));
                    slot = done
                        .wait_timeout(slot, wait)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            })
        };
        Ok(Process {
            shared,
            stdin,
            stdout,
            watchdog,
        })
    }

    /// One request: the kind and the text's length, then the text; the answer's status and
    /// length, then the tree. `None` when the worker refused the tree (past `MAX_NODES`).
    fn ask(&mut self, kind: Kind, text: &str) -> anyhow::Result<Option<scraper::Html>> {
        let process = self.process.as_mut().expect("a running worker");
        let mut head = [0u8; 5];
        head[0] = kind as u8;
        head[1..].copy_from_slice(&u32::try_from(text.len())?.to_le_bytes());
        process.stdin.write_all(&head)?;
        process.stdin.write_all(text.as_bytes())?;
        process.stdin.flush()?;
        process.stdout.read_exact(&mut head)?;
        let len = u32::from_le_bytes(head[1..].try_into().expect("four bytes")) as usize;
        match head[0] {
            0 if len <= MAX_TREE_BYTES => {}
            ANSWER_TOO_MANY_NODES => return Ok(None),
            _ => anyhow::bail!("an answer out of bounds"),
        }
        let mut answer = vec![0; len];
        process.stdout.read_exact(&mut answer)?;
        Ok(Some(
            decode(&answer, kind).context("the parse worker's answer is malformed")?,
        ))
    }

    /// End the worker and say why it ended: closed, killed, or dead at a limit.
    fn stop(&mut self) -> &'static str {
        let Some(process) = self.process.take() else {
            return "the worker ended early";
        };
        drop(process.stdin);
        drop(process.stdout);
        let status = {
            let (child, done) = &*process.shared;
            let child = child.lock().unwrap_or_else(|e| e.into_inner()).take();
            done.notify_all();
            child.map(|mut child| child.wait())
        };
        let killed = process.watchdog.join().ok().flatten();
        if let Some(reason) = killed {
            return reason;
        }
        match status {
            Some(Ok(status)) if status.signal() == Some(libc::SIGXCPU) => "the CPU budget",
            Some(Ok(status)) if matches!(status.signal(), Some(libc::SIGABRT | libc::SIGSEGV)) => {
                "the memory limit"
            }
            _ => "the worker ended early",
        }
    }

    fn end(mut self) {
        let _ = self.stop();
    }
}

/// A worker's answer status for a tree past `MAX_NODES` (it goes on with the next request).
const ANSWER_TOO_MANY_NODES: u8 = 3;

/// The parse worker: parse each request on stdin (the kind byte, the text's length, the text)
/// under `cpu_secs` of CPU time for all of them and `PARSE_MEMORY`, and answer each on stdout
/// (a status byte, the tree's length, the tree). Ends with 0 when stdin closes, 2 when the input
/// or the limits are not as expected.
pub fn worker_main(cpu_secs: u64) -> i32 {
    let limit = |resource, soft: u64, hard: u64| {
        let limit = libc::rlimit {
            rlim_cur: soft,
            rlim_max: hard,
        };
        // SAFETY: `setrlimit` reads the limit it is given and touches nothing else.
        unsafe { libc::setrlimit(resource, &limit) == 0 }
    };
    if !limit(libc::RLIMIT_CPU, cpu_secs.max(1), cpu_secs.max(1) + 1)
        || !limit(libc::RLIMIT_AS, PARSE_MEMORY, PARSE_MEMORY)
        || !limit(libc::RLIMIT_CORE, 0, 0)
    {
        return 2;
    }
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    loop {
        let mut head = [0u8; 5];
        match stdin.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return 0,
            Err(_) => return 2,
        }
        let len = u32::from_le_bytes(head[1..].try_into().expect("four bytes")) as usize;
        let mut text = vec![0; len];
        if stdin.read_exact(&mut text).is_err() {
            return 2;
        }
        let (Some(kind), Ok(text)) = (Kind::from_byte(head[0]), std::str::from_utf8(&text)) else {
            return 2;
        };
        let html = match kind {
            Kind::Document => scraper::Html::parse_document(text),
            Kind::Fragment => scraper::Html::parse_fragment(text),
        };
        let mut answer = Vec::new();
        let status = match encode(&html, &mut answer) {
            Ok(()) => 0,
            Err(()) => {
                answer.clear();
                ANSWER_TOO_MANY_NODES
            }
        };
        let mut head = [status, 0, 0, 0, 0];
        head[1..].copy_from_slice(&(answer.len() as u32).to_le_bytes());
        if stdout
            .write_all(&head)
            .and_then(|()| stdout.write_all(&answer))
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return 2;
        }
    }
}

impl Kind {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Kind::Document),
            1 => Some(Kind::Fragment),
            _ => None,
        }
    }
}

/// Without a worker: the parser fed `text` a chunk at a time until it ends or the parse has used
/// `budget` of this thread's CPU time, then the tree within its caps.
fn parse_in_process(kind: Kind, text: &str, budget: Duration) -> anyhow::Result<scraper::Html> {
    let html = match kind {
        Kind::Document => within_budget(
            html5ever::driver::parse_document(
                scraper::HtmlTreeSink::new(scraper::Html::new_document()),
                Default::default(),
            ),
            text,
            budget,
        ),
        Kind::Fragment => within_budget(
            html5ever::driver::parse_fragment(
                scraper::HtmlTreeSink::new(scraper::Html::new_fragment()),
                Default::default(),
                QualName::new(None, html5ever::ns!(html), html5ever::local_name!("body")),
                Vec::new(),
                false,
            ),
            text,
            budget,
        ),
    };
    // The same trip the worker's answer takes: the depth and node caps apply alike.
    let mut answer = Vec::new();
    encode(&html, &mut answer).map_err(|()| TooComplex {
        reason: "more nodes than the cap",
    })?;
    decode(&answer, kind)
}

/// The CPU time this thread has used.
fn thread_cpu() -> Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` only writes the `timespec` it is given, and the thread CPU clock
    // always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    Duration::new(
        u64::try_from(now.tv_sec).unwrap_or(0),
        u32::try_from(now.tv_nsec).unwrap_or(0),
    )
}

fn within_budget<S>(mut parser: S, text: &str, budget: Duration) -> scraper::Html
where
    S: TendrilSink<html5ever::tendril::fmt::UTF8, Output = scraper::Html>,
{
    let started = thread_cpu();
    let mut at = 0;
    while at < text.len() {
        let mut end = (at + PARSE_CHUNK).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        parser.process(StrTendril::from_slice(&text[at..end]));
        at = end;
        if at < text.len() && thread_cpu().saturating_sub(started) > budget {
            tracing::warn!(
                read = at,
                bytes = text.len(),
                "an HTML parse used its CPU budget; the rest of the page is not read"
            );
            break;
        }
    }
    parser.finish()
}

/// The tree as a worker answers it: a quirks byte, the node count, then each node but the root
/// in document order as its parent's index (the root is 0), its kind and its fields. Strings
/// carry their length first. Nodes below `MAX_DEPTH` are left out; past `MAX_NODES` the answer
/// is refused. The walk follows child lists, which hold what the parser built: when it moves a
/// node's children (misnested formatting tags), ego-tree 0.10 updates the parent of only the
/// first and the last, so the parent pointers of the others are stale. The rebuilt tree's are
/// not.
fn encode(html: &scraper::Html, out: &mut Vec<u8>) -> Result<(), ()> {
    use html5ever::tree_builder::QuirksMode;
    fn string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn name(out: &mut Vec<u8>, name: &QualName) {
        string(out, name.prefix.as_ref().map_or("", |p| &**p));
        string(out, &name.ns);
        string(out, &name.local);
    }
    out.push(match html.quirks_mode {
        QuirksMode::NoQuirks => 0,
        QuirksMode::LimitedQuirks => 1,
        QuirksMode::Quirks => 2,
    });
    out.extend_from_slice(&0u32.to_le_bytes());
    let mut count = 0u32;
    let mut stack: Vec<_> = html
        .tree
        .root()
        .children()
        .rev()
        .map(|child| (child, 0u32, 1usize))
        .collect();
    while let Some((node, parent, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        if count as usize >= MAX_NODES {
            return Err(());
        }
        count += 1;
        out.extend_from_slice(&parent.to_le_bytes());
        match node.value() {
            Node::Doctype(d) => {
                out.push(1);
                string(out, &d.name);
                string(out, &d.public_id);
                string(out, &d.system_id);
            }
            Node::Comment(c) => {
                out.push(2);
                string(out, &c.comment);
            }
            Node::Text(t) => {
                out.push(3);
                string(out, &t.text);
            }
            Node::Element(e) => {
                out.push(4);
                name(out, &e.name);
                out.extend_from_slice(&(e.attrs.len() as u32).to_le_bytes());
                for (attr, value) in &e.attrs {
                    name(out, attr);
                    string(out, value);
                }
            }
            Node::ProcessingInstruction(p) => {
                out.push(5);
                string(out, &p.target);
                string(out, &p.data);
            }
            Node::Document | Node::Fragment => out.push(6),
        }
        for child in node.children().rev() {
            stack.push((child, count, depth + 1));
        }
    }
    out[1..5].copy_from_slice(&count.to_le_bytes());
    Ok(())
}

/// A tree from a worker's answer (`encode`).
fn decode(bytes: &[u8], kind: Kind) -> anyhow::Result<scraper::Html> {
    use html5ever::tree_builder::QuirksMode;
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn byte(&mut self) -> anyhow::Result<u8> {
            let (&b, rest) = self.0.split_first().context("a short answer")?;
            self.0 = rest;
            Ok(b)
        }
        fn u32(&mut self) -> anyhow::Result<u32> {
            let (n, rest) = self.0.split_first_chunk::<4>().context("a short answer")?;
            self.0 = rest;
            Ok(u32::from_le_bytes(*n))
        }
        fn string(&mut self) -> anyhow::Result<&'a str> {
            let n = self.u32()? as usize;
            let (s, rest) = self.0.split_at_checked(n).context("a short answer")?;
            self.0 = rest;
            Ok(std::str::from_utf8(s)?)
        }
        fn name(&mut self) -> anyhow::Result<QualName> {
            let prefix = self.string()?;
            let ns = self.string()?;
            let local = self.string()?;
            Ok(QualName::new(
                (!prefix.is_empty()).then(|| Prefix::from(prefix)),
                Namespace::from(ns),
                LocalName::from(local),
            ))
        }
        fn tendril(&mut self) -> anyhow::Result<StrTendril> {
            Ok(StrTendril::from_slice(self.string()?))
        }
    }
    let mut html = match kind {
        Kind::Document => scraper::Html::new_document(),
        Kind::Fragment => scraper::Html::new_fragment(),
    };
    let mut reader = Reader(bytes);
    html.quirks_mode = match reader.byte()? {
        0 => QuirksMode::NoQuirks,
        1 => QuirksMode::LimitedQuirks,
        _ => QuirksMode::Quirks,
    };
    let count = reader.u32()? as usize;
    anyhow::ensure!(count <= MAX_NODES, "more nodes than the cap");
    // Each node with its depth (the root is 0): the answer is held to `MAX_DEPTH` as the worker
    // was, whatever the worker sent.
    let mut ids = Vec::with_capacity(count + 1);
    ids.push((html.tree.root().id(), 0usize));
    for _ in 0..count {
        let (parent, depth) = *ids
            .get(reader.u32()? as usize)
            .context("a node before its parent")?;
        anyhow::ensure!(depth < MAX_DEPTH, "a node deeper than the cap");
        let node = match reader.byte()? {
            1 => Node::Doctype(Doctype {
                name: reader.tendril()?,
                public_id: reader.tendril()?,
                system_id: reader.tendril()?,
            }),
            2 => Node::Comment(Comment {
                comment: reader.tendril()?,
            }),
            3 => Node::Text(Text {
                text: reader.tendril()?,
            }),
            4 => {
                let name = reader.name()?;
                let attributes = (0..reader.u32()?)
                    .map(|_| {
                        Ok(Attribute {
                            name: reader.name()?,
                            value: reader.tendril()?,
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Node::Element(Element::new(name, attributes))
            }
            5 => Node::ProcessingInstruction(ProcessingInstruction {
                target: reader.tendril()?,
                data: reader.tendril()?,
            }),
            6 => Node::Fragment,
            _ => anyhow::bail!("an unknown node kind"),
        };
        let id = html
            .tree
            .get_mut(parent)
            .context("a lost parent")?
            .append(node)
            .id();
        ids.push((id, depth + 1));
    }
    anyhow::ensure!(reader.0.is_empty(), "bytes after the last node");
    Ok(html)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum State {
    #[default]
    Data,
    TagOpen,
    EndTagOpen,
    TagName,
    BeforeAttribute,
    AttributeName,
    AfterAttributeName,
    BeforeValue,
    Quoted(u8),
    Unquoted,
    AfterQuoted,
    SelfClosing,
    /// After `<!` and the given number of `-`.
    Markup(u8),
    /// After `<![` and the given number of bytes of `CDATA[`, in SVG or MathML.
    CdataOpen(u8),
    /// In a `CDATA` section, with the count of `]` just read (at most 2).
    Cdata(u8),
    CommentStart,
    CommentStartDash,
    /// In a comment, with the count of `-` just read (at most 2).
    Comment(u8),
    CommentEndBang,
    /// `<!x…>`, `<?…>`, `</ …>`, a doctype: up to `>`.
    Bogus,
    /// In a raw-text element, matching its end tag: 0 text, 1 after `<`, 2 after `</` and some
    /// of the name, 3 after the whole name.
    Raw(u8),
    Script(Script),
    /// After `<plaintext>`: everything is text.
    Plaintext,
}

/// The tokenizer's script states: `<!--` in a script escapes it, and `<script` inside an escape
/// escapes it twice, so that a `</script>` there does not end the script.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Script {
    Data,
    Lt,
    EndName,
    EscapeStart,
    EscapeStartDash,
    Escaped,
    EscapedDash,
    EscapedDashDash,
    EscapedLt,
    EscapedEndName,
    DoubleStart,
    Double,
    DoubleDash,
    DoubleDashDash,
    DoubleLt,
    DoubleEnd,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ns {
    Html,
    Svg,
    MathMl,
}

#[derive(Debug)]
struct Open {
    name: Box<str>,
    ns: Ns,
    /// An SVG or MathML element whose content is read as HTML.
    integration: bool,
}

/// The scan of an HTML text, fed in one piece or chunk by chunk: where it must be cut (the
/// first cap passed).
#[derive(Debug, Default)]
struct Scan {
    state: State,
    /// Bytes read so far.
    at: usize,
    tags: usize,
    open: Vec<Open>,
    attributes: usize,
    value: usize,
    tag_start: usize,
    name: String,
    end: bool,
    self_closing: bool,
    /// The current attribute's name, its first bytes (`font`'s `color`, `face`, `size`).
    attribute_name: String,
    font_breakout: bool,
    raw: Option<&'static str>,
    raw_matched: usize,
    raw_ok: bool,
    cut: Option<usize>,
}

impl Scan {
    /// Read the next bytes of the text. Once a cap is passed, nothing more is read.
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.cut.is_some() {
                return;
            }
            self.step(b);
            self.at += 1;
        }
    }

    fn stop(&mut self) {
        if self.cut.is_none() {
            self.cut = Some(self.tag_start);
        }
    }

    fn open_tag(&mut self, end: bool, b: u8) -> State {
        self.name.clear();
        self.attributes = 0;
        self.end = end;
        self.self_closing = false;
        self.font_breakout = false;
        self.name_byte(b);
        State::TagName
    }

    fn name_byte(&mut self, b: u8) {
        if self.name.len() >= MAX_NAME_BYTES {
            return self.stop();
        }
        self.name.push(b.to_ascii_lowercase() as char);
    }

    fn attribute(&mut self, b: u8) -> State {
        self.attributes += 1;
        if self.attributes > MAX_TAG_ATTRIBUTES {
            self.stop();
        }
        self.attribute_name.clear();
        self.attribute_name.push(b.to_ascii_lowercase() as char);
        State::AttributeName
    }

    /// The current attribute's name has ended.
    fn attribute_named(&mut self) {
        if matches!(self.attribute_name.as_str(), "color" | "face" | "size") {
            self.font_breakout = true;
        }
    }

    fn value_byte(&mut self) {
        self.value += 1;
        if self.value > MAX_ATTRIBUTE_BYTES {
            self.stop();
        }
    }

    /// The current node is SVG or MathML, and not one whose content is read as HTML.
    fn in_foreign(&self) -> bool {
        self.open
            .last()
            .is_some_and(|o| o.ns != Ns::Html && !o.integration)
    }

    fn push(&mut self, name: Box<str>, ns: Ns) {
        let integration = match ns {
            Ns::Html => false,
            Ns::Svg => matches!(&*name, "foreignobject" | "desc" | "title"),
            Ns::MathMl => matches!(&*name, "mi" | "mo" | "mn" | "ms" | "mtext"),
        };
        self.open.push(Open {
            name,
            ns,
            integration,
        });
        if self.open.len() > MAX_DEPTH {
            self.stop();
        }
    }

    /// Pop SVG and MathML elements down to an HTML element or an integration point.
    fn break_out(&mut self) {
        while self.in_foreign() {
            self.open.pop();
        }
    }

    /// A tag ended at the current byte (`>`).
    fn emit(&mut self) -> State {
        if self.end {
            self.end_tag();
        } else {
            self.start_tag();
        }
        self.state
    }

    fn start_tag(&mut self) {
        self.state = State::Data;
        self.tags += 1;
        if self.tags > MAX_TAGS {
            return self.stop();
        }
        let name = std::mem::take(&mut self.name);
        if self.in_foreign() {
            let breaks =
                BREAKOUT.contains(&name.as_str()) || (name == "font" && self.font_breakout);
            if !breaks {
                if !self.self_closing {
                    let ns = self.open.last().map_or(Ns::Html, |o| o.ns);
                    self.push(name.into(), ns);
                }
                return;
            }
            self.break_out();
        }
        match name.as_str() {
            "svg" | "math" => {
                if !self.self_closing {
                    let ns = if name == "svg" { Ns::Svg } else { Ns::MathMl };
                    self.push(name.into(), ns);
                }
            }
            "plaintext" => self.state = State::Plaintext,
            "script" => {
                self.raw = Some("script");
                self.state = State::Script(Script::Data);
            }
            _ => {
                if let Some(raw) = RAW.iter().find(|r| **r == name) {
                    self.raw = Some(raw);
                    self.state = State::Raw(0);
                } else if !VOID.contains(&name.as_str()) && !IMPLIED.contains(&name.as_str()) {
                    self.push(name.into(), Ns::Html);
                }
            }
        }
    }

    fn end_tag(&mut self) {
        self.state = State::Data;
        self.raw = None;
        let name = std::mem::take(&mut self.name);
        // End tags follow the SVG and MathML rules under any SVG or MathML element, an
        // integration point too: only start tags and text there are read as HTML.
        if self.open.last().is_some_and(|o| o.ns != Ns::Html) {
            if name == "br" || name == "p" {
                self.break_out();
            } else {
                // From the current node down: a foreign element of this name closes; an HTML one
                // hands the tag to the HTML rules.
                for i in (0..self.open.len()).rev() {
                    if self.open[i].ns == Ns::Html {
                        break;
                    }
                    if *self.open[i].name == *name {
                        self.open.truncate(i);
                        return;
                    }
                }
            }
        }
        self.html_end_tag(&name);
    }

    fn html_end_tag(&mut self, name: &str) {
        let boundary = |o: &Open| o.integration || (o.ns == Ns::Html && SCOPE.contains(&&*o.name));
        let heading = |n: &str| matches!(n, "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
        if SCOPED_END.contains(&name) || FORMATTING.contains(&name) {
            for i in (0..self.open.len()).rev() {
                let o = &self.open[i];
                let same =
                    o.ns == Ns::Html && (*o.name == *name || (heading(name) && heading(&o.name)));
                if same {
                    self.open.truncate(i);
                    return;
                }
                if boundary(o) {
                    return;
                }
            }
            return;
        }
        // Any other end tag closes the nearest element of its name, but not past a special one.
        for i in (0..self.open.len()).rev() {
            let o = &self.open[i];
            if o.ns == Ns::Html && *o.name == *name {
                self.open.truncate(i);
                return;
            }
            if o.integration || (o.ns == Ns::Html && SPECIAL.contains(&&*o.name)) {
                return;
            }
        }
    }

    fn step(&mut self, b: u8) {
        use State::*;
        let space = matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c');
        self.state = match self.state {
            Data => {
                if b == b'<' {
                    self.tag_start = self.at;
                    TagOpen
                } else {
                    Data
                }
            }
            TagOpen => match b {
                b'!' => Markup(0),
                b'/' => EndTagOpen,
                b'?' => Bogus,
                b if b.is_ascii_alphabetic() => self.open_tag(false, b),
                _ => return self.reconsume(Data, b),
            },
            EndTagOpen => match b {
                b if b.is_ascii_alphabetic() => self.open_tag(true, b),
                b'>' => Data,
                _ => Bogus,
            },
            TagName => match b {
                b'>' => self.emit(),
                b'/' => SelfClosing,
                _ if space => BeforeAttribute,
                b => {
                    self.name_byte(b);
                    TagName
                }
            },
            BeforeAttribute => match b {
                _ if space => BeforeAttribute,
                b'/' => SelfClosing,
                b'>' => self.emit(),
                b => self.attribute(b),
            },
            AttributeName => match b {
                _ if space => {
                    self.attribute_named();
                    AfterAttributeName
                }
                b'/' => {
                    self.attribute_named();
                    SelfClosing
                }
                b'=' => {
                    self.attribute_named();
                    BeforeValue
                }
                b'>' => {
                    self.attribute_named();
                    self.emit()
                }
                b => {
                    if self.attribute_name.len() < 8 {
                        self.attribute_name.push(b.to_ascii_lowercase() as char);
                    }
                    AttributeName
                }
            },
            AfterAttributeName => match b {
                _ if space => AfterAttributeName,
                b'/' => SelfClosing,
                b'=' => BeforeValue,
                b'>' => self.emit(),
                b => self.attribute(b),
            },
            BeforeValue => match b {
                _ if space => BeforeValue,
                b'"' | b'\'' => {
                    self.value = 0;
                    Quoted(b)
                }
                b'>' => self.emit(),
                _ => {
                    self.value = 0;
                    self.value_byte();
                    Unquoted
                }
            },
            Quoted(quote) => {
                if b == quote {
                    AfterQuoted
                } else {
                    self.value_byte();
                    Quoted(quote)
                }
            }
            Unquoted => match b {
                _ if space => BeforeAttribute,
                b'>' => self.emit(),
                _ => {
                    self.value_byte();
                    Unquoted
                }
            },
            AfterQuoted => match b {
                _ if space => BeforeAttribute,
                b'/' => SelfClosing,
                b'>' => self.emit(),
                _ => return self.reconsume(BeforeAttribute, b),
            },
            SelfClosing => match b {
                b'>' => {
                    self.self_closing = true;
                    self.emit()
                }
                _ => return self.reconsume(BeforeAttribute, b),
            },
            Markup(0) => match b {
                b'-' => Markup(1),
                b'[' if self.open.last().is_some_and(|o| o.ns != Ns::Html) => CdataOpen(1),
                _ => return self.reconsume(Bogus, b),
            },
            Markup(_) => match b {
                b'-' => CommentStart,
                _ => return self.reconsume(Bogus, b),
            },
            CdataOpen(n) => {
                if b"[CDATA[".get(usize::from(n)) == Some(&b) {
                    if n == 6 {
                        Cdata(0)
                    } else {
                        CdataOpen(n + 1)
                    }
                } else {
                    return self.reconsume(Bogus, b);
                }
            }
            Cdata(brackets) => match b {
                b']' => Cdata((brackets + 1).min(2)),
                b'>' if brackets == 2 => Data,
                _ => Cdata(0),
            },
            CommentStart => match b {
                b'-' => CommentStartDash,
                b'>' => Data,
                _ => Comment(0),
            },
            CommentStartDash => match b {
                b'-' => Comment(2),
                b'>' => Data,
                _ => Comment(0),
            },
            Comment(dashes) => match b {
                b'-' => Comment((dashes + 1).min(2)),
                b'>' if dashes >= 2 => Data,
                b'!' if dashes >= 2 => CommentEndBang,
                _ => Comment(0),
            },
            CommentEndBang => match b {
                b'-' => Comment(1),
                b'>' => Data,
                _ => Comment(0),
            },
            Bogus => {
                if b == b'>' {
                    Data
                } else {
                    Bogus
                }
            }
            Raw(stage) => self.raw_step(stage, b, space),
            Script(state) => self.script_step(state, b, space),
            Plaintext => Plaintext,
        };
    }

    /// Read `b` again in `state`.
    fn reconsume(&mut self, state: State, b: u8) {
        self.state = state;
        self.step(b);
    }

    /// The raw-text element's end tag has matched up to `b`, a white space, `/` or `>`: it is
    /// an end tag like any other from here (attributes and all).
    fn raw_end(&mut self, b: u8, space: bool) -> State {
        self.name.clear();
        self.name.push_str(self.raw.unwrap_or(""));
        self.attributes = 0;
        self.end = true;
        self.self_closing = false;
        if b == b'>' {
            self.emit()
        } else if space {
            State::BeforeAttribute
        } else {
            State::SelfClosing
        }
    }

    fn raw_step(&mut self, stage: u8, b: u8, space: bool) -> State {
        let name = self.raw.unwrap_or("").as_bytes();
        match stage {
            0 => {
                if b == b'<' {
                    self.tag_start = self.at;
                    State::Raw(1)
                } else {
                    State::Raw(0)
                }
            }
            1 => {
                if b == b'/' {
                    self.raw_matched = 0;
                    State::Raw(2)
                } else {
                    self.reconsume(State::Raw(0), b);
                    self.state
                }
            }
            2 => {
                if name.get(self.raw_matched) == Some(&b.to_ascii_lowercase()) {
                    self.raw_matched += 1;
                    if self.raw_matched == name.len() {
                        State::Raw(3)
                    } else {
                        State::Raw(2)
                    }
                } else {
                    self.reconsume(State::Raw(0), b);
                    self.state
                }
            }
            _ => {
                if b == b'>' || b == b'/' || space {
                    self.raw_end(b, space)
                } else {
                    self.reconsume(State::Raw(0), b);
                    self.state
                }
            }
        }
    }

    /// Match `b` against `script`, case-insensitively: false once the name differs.
    fn script_name(&mut self, b: u8) {
        self.raw_ok =
            self.raw_ok && b"script".get(self.raw_matched) == Some(&b.to_ascii_lowercase());
        self.raw_matched += 1;
    }

    fn script_name_is_script(&self) -> bool {
        self.raw_ok && self.raw_matched == 6
    }

    fn script_step(&mut self, state: Script, b: u8, space: bool) -> State {
        use Script::*;
        let again = |scan: &mut Self, state: Script| {
            scan.reconsume(State::Script(state), b);
            scan.state
        };
        let lt = |scan: &mut Self, next: Script| {
            scan.tag_start = scan.at;
            State::Script(next)
        };
        let next = match state {
            Data => match b {
                b'<' => return lt(self, Lt),
                _ => Data,
            },
            Lt => match b {
                b'/' => {
                    self.raw_matched = 0;
                    self.raw_ok = true;
                    EndName
                }
                b'!' => EscapeStart,
                _ => return again(self, Data),
            },
            EndName | EscapedEndName => {
                let base = if state == EndName { Data } else { Escaped };
                if b.is_ascii_alphabetic() {
                    self.script_name(b);
                    state
                } else if (space || b == b'/' || b == b'>') && self.script_name_is_script() {
                    return self.raw_end(b, space);
                } else {
                    return again(self, base);
                }
            }
            EscapeStart => match b {
                b'-' => EscapeStartDash,
                _ => return again(self, Data),
            },
            EscapeStartDash => match b {
                b'-' => EscapedDashDash,
                _ => return again(self, Data),
            },
            Escaped => match b {
                b'-' => EscapedDash,
                b'<' => return lt(self, EscapedLt),
                _ => Escaped,
            },
            EscapedDash => match b {
                b'-' => EscapedDashDash,
                b'<' => return lt(self, EscapedLt),
                _ => Escaped,
            },
            EscapedDashDash => match b {
                b'-' => EscapedDashDash,
                b'<' => return lt(self, EscapedLt),
                b'>' => Data,
                _ => Escaped,
            },
            EscapedLt => match b {
                b'/' => {
                    self.raw_matched = 0;
                    self.raw_ok = true;
                    EscapedEndName
                }
                b if b.is_ascii_alphabetic() => {
                    self.raw_matched = 0;
                    self.raw_ok = true;
                    return again(self, DoubleStart);
                }
                _ => return again(self, Escaped),
            },
            DoubleStart | DoubleEnd => {
                let (matched, missed) = if state == DoubleStart {
                    (Double, Escaped)
                } else {
                    (Escaped, Double)
                };
                if b.is_ascii_alphabetic() {
                    self.script_name(b);
                    state
                } else if space || b == b'/' || b == b'>' {
                    if self.script_name_is_script() {
                        matched
                    } else {
                        missed
                    }
                } else {
                    return again(self, missed);
                }
            }
            Double => match b {
                b'-' => DoubleDash,
                b'<' => DoubleLt,
                _ => Double,
            },
            DoubleDash => match b {
                b'-' => DoubleDashDash,
                b'<' => DoubleLt,
                _ => Double,
            },
            DoubleDashDash => match b {
                b'-' => DoubleDashDash,
                b'<' => DoubleLt,
                b'>' => Data,
                _ => Double,
            },
            DoubleLt => match b {
                b'/' => {
                    self.raw_matched = 0;
                    self.raw_ok = true;
                    DoubleEnd
                }
                _ => return again(self, Double),
            },
        };
        State::Script(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(text: &str) -> &str {
        bounded(text)
    }
    /// `f` within a few seconds of this thread's CPU time (a bound load never inflates; the
    /// shapes here take milliseconds, the quadratic ones they guard against took minutes).
    fn timed<T>(f: impl FnOnce() -> T) -> T {
        let started = crate::capability::thread_cpu_time();
        let out = f();
        let used = crate::capability::thread_cpu_time() - started;
        assert!(used < std::time::Duration::from_secs(5), "{used:?}");
        out
    }
    fn title(html: &scraper::Html) -> Option<String> {
        let title = scraper::Selector::parse("title").unwrap();
        html.select(&title).next().map(|t| t.text().collect())
    }

    #[test]
    fn real_markup_is_left_whole() {
        let mut attributes = String::from("<div");
        for n in 0..MAX_TAG_ATTRIBUTES {
            attributes.push_str(&format!(" data-{n}=\"a b c\""));
        }
        attributes.push('>');
        let list = format!("<ul>{}</ul>", "<li>item".repeat(2_000));
        let nested = format!(
            "{}x{}",
            "<div>".repeat(MAX_DEPTH),
            "</div>".repeat(MAX_DEPTH)
        );
        for text in [
            "<html><head><title>T</title></head><body><p class='a b c'>x</p></body></html>",
            "<script>if (a < b && c<d) { x = '<div a b c d>'; }</script><p>y</p>",
            "<!-- <div a b c --><p>z</p>",
            "a < b, and <unclosed",
            attributes.as_str(),
            list.as_str(),
            nested.as_str(),
            // Stray end tags close nothing.
            "</div></span><div>ok</div>",
        ] {
            assert_eq!(cut(text), text);
        }
    }

    /// Each of Codex's probes is cut where it passes a cap, and parses in well under a second.
    #[test]
    fn hostile_markup_is_cut_at_its_cap() {
        // One tag with 60,000 attributes, separated by spaces or by `/`.
        let mut spaced = String::from("<title>Kept</title><div");
        let mut slashed = String::from("<title>Kept</title><div");
        for n in 0..60_000 {
            spaced.push_str(&format!(" a{n}='x'"));
            slashed.push_str(&format!("/a{n}='x'"));
        }
        // Attributes run together after a quoted value.
        let mut joined = String::from("<title>Kept</title><div a0='x'");
        for n in 1..60_000 {
            joined.push_str(&format!("a{n}='x'"));
        }
        // 20,000 unclosed divs; 30,000 `<span>x`; spans closed by end tags of other names.
        let unclosed = format!("<title>Kept</title>{}", "<div>".repeat(20_000));
        let spans = format!("<title>Kept</title>{}", "<span>x".repeat(30_000));
        let stray = format!("<title>Kept</title>{}", "<span></x>".repeat(30_000));
        // 13,000 divs with no closing tags behind a comment that closes at once.
        let comment = format!("<title>Kept</title><!-->{}", "<div>".repeat(13_000));
        let many = format!("<title>Kept</title>{}", "<br>".repeat(60_000));
        for page in [
            &spaced, &slashed, &joined, &unclosed, &spans, &stray, &comment, &many,
        ] {
            assert!(cut(page).len() < page.len());
            let html = timed(|| document(page).unwrap());
            assert_eq!(title(&html).as_deref(), Some("Kept"));
        }
    }

    /// The second round of Codex's probes: attributes on an end tag, a tag inside SVG's
    /// `<title>`, names alike in their first 32 bytes, and end tags that cannot close past a
    /// special element. Each is cut at its cap and parses at once.
    #[test]
    fn markup_the_scan_once_misread_is_cut() {
        let attributes = |tag: &str| {
            let mut text = format!("<title>Kept</title>{tag}");
            for n in 0..60_000 {
                text.push_str(&format!("/a{n}='x'"));
            }
            text.push('>');
            text
        };
        let end_tag = attributes("</div");
        let svg_title = attributes("<svg><title><div");
        // A `CDATA` section in SVG holds a `>` and a quote the scan must not read as markup.
        let cdata = attributes("<svg><![CDATA[ > <x a=' ]]><div");
        // An end tag's quoted value holds a `>`.
        let end_quoted = attributes("<p></p a='>'><div");
        let long = "a".repeat(32);
        let alike = format!(
            "<title>Kept</title>{}",
            format!("<{long}x></{long}y>").repeat(14_000)
        );
        let special = format!("<title>Kept</title>{}", "<x><div></x>".repeat(20_000));
        let named = format!("<title>Kept</title><{}>", "a".repeat(MAX_NAME_BYTES + 1));
        for page in [
            &end_tag,
            &svg_title,
            &cdata,
            &end_quoted,
            &alike,
            &special,
            &named,
        ] {
            assert!(cut(page).len() < page.len(), "{}", &page[..60]);
            let html = timed(|| document(page).unwrap());
            assert_eq!(title(&html).as_deref(), Some("Kept"), "{}", &page[..60]);
        }
    }

    /// SVG and MathML read as the parser reads them: `<path/>` closes itself, `<div>` ends the
    /// SVG, and what follows is read as HTML again.
    #[test]
    fn svg_and_mathml_are_read_as_the_parser_reads_them() {
        let paths = "<path d='M0 0 L1 1'/>".repeat(300);
        let ld = r#"<script type="application/ld+json">{"@type":"Event","name":"After"}</script>"#;
        for page in [
            format!("<title>T</title><svg viewBox='0 0 1 1'>{paths}</svg>{ld}"),
            format!("<title>T</title><svg>{paths}<div>broke out</div>{ld}"),
            format!("<title>T</title><math><mi>x</mi><mo>+</mo>{}</math>{ld}", "<mn/>".repeat(300)),
            format!("<title>T</title><svg><title>icon</title><desc><style>a<b{{}}</style></desc>{paths}</svg>{ld}"),
        ] {
            assert_eq!(cut(&page), page);
            let html = document(&page).unwrap();
            let script = scraper::Selector::parse("script").unwrap();
            assert_eq!(html.select(&script).count(), 1, "{page}");
        }
    }

    /// The parser reads at most its budget of CPU time, whatever the scan let through, and keeps
    /// what it read.
    #[test]
    fn a_parse_stops_at_its_budget() {
        let mut slashed = String::from("<title>Kept</title></div");
        for n in 0..60_000 {
            slashed.push_str(&format!("/a{n}='x'"));
        }
        let started = crate::capability::thread_cpu_time();
        let html = parse_in_process(Kind::Document, &slashed, Duration::from_millis(50)).unwrap();
        // The budget is of this thread's CPU time: the tree is built soon after it.
        let used = crate::capability::thread_cpu_time() - started;
        assert!(used < Duration::from_secs(3), "{used:?}");
        assert_eq!(title(&html).as_deref(), Some("Kept"));
    }

    /// A tree deeper than `MAX_DEPTH` loses what is below it; `select` meets no deeper node.
    #[test]
    fn a_parsed_tree_stays_within_the_depth_cap() {
        let deep = format!(
            "<title>Kept</title>{}<b>deep</b>",
            "<span>".repeat(2 * MAX_DEPTH)
        );
        let html = parse_in_process(Kind::Document, &deep, PARSE_BUDGET).unwrap();
        assert_eq!(title(&html).as_deref(), Some("Kept"));
        let any = scraper::Selector::parse("*").unwrap();
        let deepest = html.select(&any).map(|e| e.ancestors().count()).max();
        // An element's ancestors count the document too.
        assert!(deepest.is_some_and(|d| d <= MAX_DEPTH), "{deepest:?}");
        let bold = scraper::Selector::parse("b").unwrap();
        assert_eq!(html.select(&bold).count(), 0);
    }

    /// A tree the parser rearranged (misnested `<b>` moves a `<p>`'s children) comes back with
    /// every parent pointer right: the parser's own tree leaves the middle children pointing at
    /// their old parent, and `ancestors()` there walks the wrong way.
    #[test]
    fn a_rebuilt_tree_has_its_parents_right() {
        fn names(node: ego_tree::NodeRef<'_, Node>, out: &mut Vec<String>) {
            for child in node.children() {
                out.push(match child.value() {
                    Node::Element(e) => e.name().to_owned(),
                    Node::Text(t) => t.text.to_string(),
                    _ => String::new(),
                });
                names(child, out);
            }
        }
        let stale = |html: &scraper::Html| {
            html.tree
                .root()
                .descendants()
                .flat_map(|n| n.children().map(move |c| (n.id(), c)))
                .filter(|(id, c)| c.parent().map(|p| p.id()) != Some(*id))
                .count()
        };
        let page = "<b>1<p>2<i>a</i><i>b</i><i>c</i><i>d</i></b>after";
        let plain = scraper::Html::parse_document(page);
        let rebuilt = parse_in_process(Kind::Document, page, PARSE_BUDGET).unwrap();
        assert!(
            stale(&plain) > 0,
            "ego-tree fixed its reparenting: this test can go"
        );
        assert_eq!(stale(&rebuilt), 0);
        let (mut a, mut b) = (vec![], vec![]);
        names(plain.tree.root(), &mut a);
        names(rebuilt.tree.root(), &mut b);
        assert_eq!(a, b, "the same tree, by child lists");
    }

    /// Plain text passes the job's checks like markup: a job whose caller is gone, whose deadline
    /// or CPU budget has passed, parses no more plain text either (Codex on 6877c1f). A job still
    /// within its limits parses it.
    #[test]
    fn a_job_that_is_over_refuses_plain_text_too() {
        let plain = "a plain description, no markup";
        let mut fresh = Job::new(PARSE_BUDGET, Arc::default());
        assert!(fresh.parse(Kind::Fragment, plain).is_ok());
        let mut cancelled = Job::new(PARSE_BUDGET, Arc::new(AtomicBool::new(true)));
        let mut late = Job::new(PARSE_BUDGET, Arc::default());
        late.deadline = Instant::now();
        let mut spent = Job::new(PARSE_BUDGET, Arc::default());
        spent.budget = Duration::ZERO;
        for (job, reason) in [
            (&mut cancelled, "the caller stopped waiting"),
            (&mut late, "the wall deadline"),
            (&mut spent, "the CPU budget"),
        ] {
            for _ in 0..2 {
                let error = job.parse(Kind::Fragment, plain).unwrap_err();
                let error = error.downcast_ref::<TooComplex>().expect("too complex");
                assert_eq!(error.reason, reason);
            }
        }
    }

    /// Work in a job besides parsing sees the job end: `job_over` is false outside a job and in a
    /// live one, true once its caller is gone or its deadline passed, and a feed's prefix stops
    /// there.
    #[test]
    fn a_feed_prefix_stops_when_its_job_is_over() {
        let feed = "<rss><channel><item><title>1</title></item><item><title>2";
        assert!(!job_over());
        let live = in_job(Job::new(PARSE_BUDGET, Arc::default()), || {
            (job_over(), crate::resolver::cards::feed_prefix(feed, 10))
        });
        assert!(!live.0);
        assert!(live.1.is_some());
        let gone = in_job(
            Job::new(PARSE_BUDGET, Arc::new(AtomicBool::new(true))),
            || (job_over(), crate::resolver::cards::feed_prefix(feed, 10)),
        );
        assert_eq!(gone, (true, None));
        let mut late = Job::new(PARSE_BUDGET, Arc::default());
        late.deadline = Instant::now();
        assert!(in_job(late, job_over));
    }

    /// A worker's answer is held to `MAX_DEPTH` when it is read back, whatever the worker sent (a
    /// buggy or hostile one): a chain of `MAX_DEPTH` elements is read, one more is refused.
    #[test]
    fn a_worker_answer_deeper_than_the_cap_is_refused() {
        let chain = |nodes: u32| {
            let mut bytes = vec![0u8];
            bytes.extend_from_slice(&nodes.to_le_bytes());
            for parent in 0..nodes {
                bytes.extend_from_slice(&parent.to_le_bytes());
                bytes.push(4);
                for s in ["", "http://www.w3.org/1999/xhtml", "div"] {
                    bytes.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    bytes.extend_from_slice(s.as_bytes());
                }
                bytes.extend_from_slice(&0u32.to_le_bytes());
            }
            bytes
        };
        assert!(decode(&chain(MAX_DEPTH as u32), Kind::Document).is_ok());
        let error = decode(&chain(MAX_DEPTH as u32 + 1), Kind::Document).unwrap_err();
        assert!(
            format!("{error:#}").contains("deeper than the cap"),
            "{error:#}"
        );
    }

    /// A worker that does not start is tried once per job: the job's later parses fail at once,
    /// and its plain text still parses.
    #[test]
    fn a_worker_that_does_not_start_is_tried_once_per_job() {
        let missing = Worker {
            program: "/nonexistent/pinakes-parse".into(),
            args: vec![],
        };
        let mut job = Job::new(PARSE_BUDGET, Arc::default());
        let first = job
            .parse_with(Kind::Fragment, "<b>x</b>", Some(&missing))
            .unwrap_err();
        assert!(!first.is::<TooComplex>(), "{first:#}");
        assert!(job.start_failed);
        let again = job
            .parse_with(Kind::Fragment, "<b>y</b>", Some(&missing))
            .unwrap_err();
        assert_eq!(format!("{again:#}"), "the parse worker did not start");
        assert!(job.process.is_none());
        assert!(job
            .parse_with(Kind::Fragment, "plain", Some(&missing))
            .is_ok());
    }

    /// A CPU budget no parse of a test reaches: a page cut short at the budget is a smaller tree,
    /// so a test of the node cap parses with this budget and the cap alone can stop it.
    const AMPLE_BUDGET: Duration = Duration::from_secs(60 * 60);

    /// A tree refused at `MAX_NODES` is logged once per job, however many the job refuses, and
    /// the job goes on.
    #[test]
    fn a_tree_past_the_node_cap_is_logged_once_per_job() {
        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .finish();
        let page = format!("<title>T</title>{}", "<b></b>".repeat(MAX_NODES));
        tracing::subscriber::with_default(subscriber, || {
            let mut job = Job::new(AMPLE_BUDGET, Arc::default());
            for _ in 0..2 {
                let error = job.parse_with(Kind::Document, &page, None).unwrap_err();
                assert!(error.is::<TooComplex>());
            }
            assert!(job.parse_with(Kind::Fragment, "<b>x</b>", None).is_ok());
        });
        let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            text.matches("did not end within its limits").count(),
            1,
            "{text}"
        );
    }

    /// A tree past `MAX_NODES` is refused as too complex, in this process as in a worker.
    #[test]
    fn a_tree_past_the_node_cap_is_too_complex() {
        let page = format!("<title>T</title>{}", "<b></b>".repeat(MAX_NODES));
        let error = parse_in_process(Kind::Document, &page, AMPLE_BUDGET).unwrap_err();
        assert!(error.is::<TooComplex>(), "{error:#}");
        let within = format!("<title>T</title>{}", "<b></b>".repeat(MAX_NODES - 8));
        assert!(parse_in_process(Kind::Document, &within, AMPLE_BUDGET).is_ok());
    }

    /// Fed in pieces, the parser builds the tree it builds from one piece.
    #[test]
    fn a_parse_in_pieces_builds_the_same_tree() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut stack = vec![fixtures];
        let mut pages = vec![
            format!("<p>{}</p>", "é漢🙂".repeat(1_000)),
            format!("<script>{}</script><p>x", "if (a<b) {}".repeat(500)),
        ];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "html") {
                    pages.push(std::fs::read_to_string(&path).unwrap());
                }
            }
        }
        assert!(pages.len() > 20);
        for page in &pages {
            let whole = scraper::Html::parse_document(page);
            let pieces = parse_in_process(Kind::Document, page, PARSE_BUDGET).unwrap();
            assert_eq!(pieces.root_element().html(), whole.root_element().html());
            let whole = scraper::Html::parse_fragment(page);
            let pieces = parse_in_process(Kind::Fragment, page, PARSE_BUDGET).unwrap();
            assert_eq!(pieces.root_element().html(), whole.root_element().html());
        }
    }

    /// Codex's probes, each through the code that met it, finish in well under a second (in a
    /// debug build) and overflow no stack.
    #[test]
    fn codex_s_probes_finish_at_once() {
        use crate::resolver::{egress::Fetched, interstitial, web};
        let fetched = |status: u16, content_type: &str, body: String| Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: "https://hostile.example.org/p".parse().unwrap(),
            content_type: content_type.into(),
            status,
            retry_after_secs: 0,
            location: None,
            body: body.into_bytes(),
        };
        let url: url::Url = "https://hostile.example.org/p".parse().unwrap();
        // 10,000 nested spans in an Internet Archive description: plain text, no overflow.
        let nested = format!(
            "<p>Kept</p>{}deep{}",
            "<span>".repeat(10_000),
            "</span>".repeat(10_000)
        );
        let text = timed(|| crate::sources::kit::html_text(&nested));
        assert_eq!(text.as_deref(), Some("Kept"));
        // 20,000 unclosed divs; a tag with 60,000 attributes separated by `/`.
        let unclosed = format!("<title>Kept</title>{}", "<div>".repeat(20_000));
        let mut slashed = String::from("<title>Kept</title><div");
        for n in 0..60_000 {
            slashed.push_str(&format!("/a{n}='x'"));
        }
        for page in [unclosed, slashed] {
            let out = timed(|| {
                web::extract(url.as_str(), &fetched(200, "text/html", page), 3600).unwrap()
            });
            assert_eq!(out.doc.name.or(out.title).as_deref(), Some("Kept"));
        }
        // Repeated `<span>x` nesting against the wall check.
        let spans = format!("<title>Kept</title>{}", "<span>x".repeat(30_000));
        timed(|| interstitial::inspect(&url, &fetched(200, "text/html", spans)).unwrap());
        // Nested JSON-LD scripts.
        let ld = r#"<script type="application/ld+json">{"@type":"Event","name":"x"}</script>"#;
        let json_ld = format!("<title>Kept</title>{}", format!("<div>{ld}").repeat(20_000));
        timed(|| web::extract(url.as_str(), &fetched(200, "text/html", json_ld), 3600).unwrap());
        // A 503 text/plain answer of 13,000 divs, read by the wall check.
        let plain = "<div>".repeat(13_000);
        timed(|| interstitial::inspect(&url, &fetched(503, "text/plain", plain)).unwrap());
    }

    #[tokio::test]
    async fn a_parse_holds_its_permit_until_it_ends() {
        let before = running();
        let (started, wait) = std::sync::mpsc::channel();
        let (finish, done) = std::sync::mpsc::channel::<()>();
        let parse = tokio::spawn(blocking(move || {
            started.send(()).unwrap();
            done.recv().unwrap();
        }));
        tokio::task::spawn_blocking(move || wait.recv().unwrap())
            .await
            .unwrap();
        // The caller stops waiting; the parse still runs and still holds its permit.
        parse.abort();
        assert!(running() > before);
        finish.send(()).unwrap();
        for _ in 0..200 {
            if running() == before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(running(), before);
    }

    /// While every permit is taken elsewhere in the process (another test's parses), a paused
    /// test waits for one without its clock moving: a fetch timeout around the wait doesn't
    /// pass in no time.
    #[tokio::test(start_paused = true)]
    async fn a_wait_for_a_permit_leaves_a_paused_clock_alone() {
        let (taken, wait) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let all = runtime
                .block_on(PARSES.clone().acquire_many_owned(*PERMITS as u32))
                .unwrap();
            taken.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(all);
        });
        tokio::task::spawn_blocking(move || wait.recv().unwrap())
            .await
            .unwrap();
        let start = tokio::time::Instant::now();
        let parsed = tokio::time::timeout(std::time::Duration::from_secs(10), blocking(|| 1)).await;
        assert_eq!(parsed.expect("the wait passed the timeout").unwrap(), 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        holder.join().unwrap();
    }

    /// No code but this module names the HTML parser's entry points.
    #[test]
    fn only_this_module_parses_html() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        let mut found = vec![];
        let entry = [
            ["Html::", "parse_document"].concat(),
            ["Html::", "parse_fragment"].concat(),
            ["Html::", "new_document"].concat(),
            ["Html::", "new_fragment"].concat(),
            ["html5ever", "::"].concat(),
            ["parse_document", "(scraper"].concat(),
            ["html::", "off_runtime("].concat(),
        ];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "rs")
                    && !path.ends_with("resolver/html.rs")
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for name in &entry {
                        if text.contains(name.as_str()) {
                            found.push(format!("{}: {name}", path.display()));
                        }
                    }
                }
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }
}
