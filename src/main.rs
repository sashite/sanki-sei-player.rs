//! `sanki-sei-player` — an engine for the Sashité Engine Interface (SEI
//! 1.0.0, <https://sashite.dev/specs/sei/1.0.0/>) that plays Sanki
//! (`sashite.sanki.kernel/1`) with the reference search of
//! `sashite-sanki-player`, under SEI's clock.
//!
//! The transport, the envelope, `hello`, `ping`, `configure`, `cancel`, the
//! errors and their classes, and the validation of `search` under the rules
//! document are those of `sanki-sei-random-engine`, the engine this one is
//! forked from. What differs is that a search takes time: it runs on a
//! **worker thread**, so that the main thread keeps reading the input —
//! `ping` is answered at its receipt (§7 rule 3), and any other request
//! ends the active search first (§7 rule 2), by raising the interruption
//! flag the brain polls and waiting for its last iteration. Both threads
//! write events through one lock, one line at a time.
//!
//! An infinite search (neither `clock` nor `limits`) runs to the configured
//! depth and keeps its `done` until the next request, which ends it.

#![forbid(unsafe_code)]

mod protocol;
mod search;
mod think;

use std::io::{self, BufRead};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use protocol::{Envelope, Event, Options, Request};

/// Lines longer than this are refused as an envelope error (SEI §5 asks every
/// receiver to accept at least 1 MiB; we accept four).
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// The output, shared by the two threads: one event per lock.
type Out = Arc<Mutex<io::Stdout>>;

/// Writes one event line. `Err(())` when the output is gone.
fn emit(out: &Out, event: &Event) -> Result<(), ()> {
    let mut stdout = out.lock().unwrap_or_else(PoisonError::into_inner);
    event.write(&mut *stdout)
}

/// How a search ended, as its thread reports it.
enum Outcome {
    /// The `done` was written.
    Written,
    /// The search was infinite: its `done` waits for the next request.
    Held(Event),
    /// The output is gone: the session is over.
    Closed,
}

/// A search on its thread.
struct Worker {
    /// The interruption the brain polls.
    cancelled: Arc<AtomicBool>,
    handle: JoinHandle<Outcome>,
}

/// The engine's whole state: what `hello` and `configure` established, and
/// the active search.
struct Engine {
    /// The last `id` of a request whose envelope was valid.
    last_id: Option<i64>,
    /// Whether a `hello` has succeeded.
    opened: bool,
    /// The effective configuration (§8.3).
    options: Options,
    /// The active search, running or holding its `done`.
    worker: Option<Worker>,
}

impl Engine {
    /// Ends the active search, if any: interrupts it, waits for its last
    /// iteration, writes the `done` it held. `Err(())` when the output is
    /// gone or the search failed.
    fn end_search(&mut self, out: &Out) -> Result<(), ()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.cancelled.store(true, Ordering::Relaxed);
        match worker.handle.join() {
            Ok(Outcome::Written) => Ok(()),
            Ok(Outcome::Held(done)) => emit(out, &done),
            Ok(Outcome::Closed) => Err(()),
            Err(_) => {
                // The brain panicked: fatal, said before the exit.
                let _ = emit(out, &Event::fatal("internal", "the search thread failed"));
                Err(())
            }
        }
    }
}

fn main() -> ExitCode {
    let stdin = io::stdin();
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let mut engine = Engine {
        last_id: None,
        opened: false,
        options: Options::default(),
        worker: None,
    };
    let mut line = String::new();
    loop {
        line.clear();
        // `read_line` reads to the LF whatever the length: the bound is
        // checked after, and a line beyond it is an envelope error.
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                // End of input: the host closed the session. The search is
                // interrupted, and its result — held or written — is the
                // host's to ignore.
                let _ = engine.end_search(&out);
                return ExitCode::SUCCESS;
            }
            Ok(_) => {}
            Err(_) => return fatal(&mut engine, &out, "invalid", "invalid encoding"),
        }
        let received = Instant::now();
        let text = line.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_LINE_BYTES {
            return fatal(&mut engine, &out, "invalid", "line longer than 4 MiB");
        }
        match protocol::parse(text, engine.last_id, engine.opened) {
            Err(Envelope(message)) => return fatal(&mut engine, &out, "invalid", &message),
            Ok(request) => {
                engine.last_id = Some(request.id);
                if let Err(()) = handle(&mut engine, request, received, &out) {
                    return ExitCode::FAILURE;
                }
            }
        }
    }
}

/// Ends the active search, emits the fatal error (no `re`) and returns the
/// failure exit code (SEI §5, §7 rule 7).
fn fatal(engine: &mut Engine, out: &Out, code: &str, message: &str) -> ExitCode {
    let _ = engine.end_search(out);
    let _ = emit(out, &Event::fatal(code, message));
    ExitCode::FAILURE
}

/// Processes one request whose envelope is valid. `Err(())` means the output
/// could not be written, or the engine failed: the session is over.
fn handle(engine: &mut Engine, request: Request, received: Instant, out: &Out) -> Result<(), ()> {
    let id = request.id;
    // `ping` stands apart: answered at once, changes nothing (§7 rule 3).
    if request.op == "ping" {
        let event = match protocol::check_ping(&request) {
            Ok(()) => Event::done(id),
            Err(error) => error.attached(id),
        };
        return emit(out, &event);
    }
    // Every other request first ends the active search (§7 rule 2).
    engine.end_search(out)?;
    let event = match request.op.as_str() {
        "hello" => match protocol::check_hello(&request) {
            Ok(common) => {
                // The opening succeeds only when `version` is returned (§7 rule 1).
                engine.opened |= common;
                protocol::hello_done(id, common)
            }
            Err(error) => error.attached(id),
        },
        "configure" => match protocol::check_configure(&request) {
            Ok(options) => {
                engine.options = options;
                Event::done(id)
            }
            Err(error) => error.attached(id),
        },
        "search" => match search::validate(&request) {
            Err(error) => error.attached(id),
            Ok(searched) if searched.terminal => think::done_terminal(id),
            Ok(searched) => {
                let plan = think::plan(&searched, engine.options, received);
                let cancelled = Arc::new(AtomicBool::new(false));
                let flag = Arc::clone(&cancelled);
                let options = engine.options;
                let out = Arc::clone(out);
                let spawned =
                    thread::Builder::new()
                        .name(format!("search-{id}"))
                        .spawn(move || {
                            run_search(id, &searched, options, &plan, received, &flag, &out)
                        });
                match spawned {
                    Ok(handle) => {
                        engine.worker = Some(Worker { cancelled, handle });
                        return Ok(());
                    }
                    // The request failed before its search started: an
                    // error attached to it (§9.4), the session going on.
                    Err(_) => {
                        protocol::Error::internal("the search thread could not start").attached(id)
                    }
                }
            }
        },
        "cancel" => match protocol::check_cancel(&request) {
            Ok(()) => Event::done(id),
            Err(error) => error.attached(id),
        },
        _ => protocol::Error::invalid("unknown operation", Some("/op")).attached(id),
    };
    emit(out, &event)
}

/// The search thread: the safety net's `info` first — the brain's depth-1
/// choice, exempt from every stop — then the search under the plan, and its
/// `done`, written or held (§8.4 *Guaranteed result*).
fn run_search(
    id: i64,
    searched: &search::Searched,
    options: Options,
    plan: &think::Plan,
    received: Instant,
    cancelled: &Arc<AtomicBool>,
    out: &Out,
) -> Outcome {
    let seed = think::seed_of(options.seed);
    let net = think::run(searched, seed, &think::Plan::safety_net(), cancelled);
    if let Some(choice) = &net {
        let info = think::info(id, &searched.position, choice, received.elapsed());
        if emit(out, &info).is_err() {
            return Outcome::Closed;
        }
    }
    // The search proper; its nodes count from the receipt, the net's included.
    let choice = think::run(searched, seed, plan, cancelled)
        .map(|mut choice| {
            choice.nodes = choice
                .nodes
                .saturating_add(net.as_ref().map_or(0, |n| n.nodes));
            choice
        })
        .or(net);
    let done =
        choice.and_then(|choice| think::done(id, &searched.position, &choice, received.elapsed()));
    let Some(done) = done else {
        // No legal move could be produced: fatal (§8.4 *Guaranteed result*).
        // The main thread waits on the input; the session ends here.
        let _ = emit(
            out,
            &Event::fatal("internal", "no legal move could be produced"),
        );
        std::process::exit(1);
    };
    if plan.infinite {
        return Outcome::Held(done);
    }
    match emit(out, &done) {
        Ok(()) => Outcome::Written,
        Err(()) => Outcome::Closed,
    }
}
