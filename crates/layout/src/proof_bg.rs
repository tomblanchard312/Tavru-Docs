//! Spelling and grammar marks computed off the layout thread (#3613).
//!
//! Layout still decides line breaks on the caller. The dictionary check for each paragraph is
//! handed to one background thread, which writes the marks into the paragraph the window is
//! already drawing. A later frame paints them. Callers that do not defer still check inline.

use std::ops::Range;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock, mpsc};
use std::thread;

use wordcraft_proof::{IssueKind, Options, ProofLang};

static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Marks a background check writes after layout has returned.
#[derive(Debug, Default)]
pub struct IssueMarks {
    issues: RwLock<Vec<(usize, usize, bool)>>,
}

impl IssueMarks {
    pub fn snapshot(&self) -> Vec<(usize, usize, bool)> {
        self.issues.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, issues: Vec<(usize, usize, bool)>) {
        *self.issues.write().unwrap_or_else(|e| e.into_inner()) = issues;
    }
}

struct Job {
    marks: Arc<IssueMarks>,
    text: String,
    spans: Vec<(Range<usize>, ProofLang)>,
    skip: Vec<Range<usize>>,
    spelling: Options,
    grammar: bool,
    list_item: bool,
    typing: Option<(usize, usize)>,
}

/// True while a deferred check has been queued and not finished.
pub fn proof_busy() -> bool {
    PENDING.load(Ordering::Acquire) > 0
}

/// Queue one paragraph's check. `None` when this build has no background thread: the caller
/// checks inline instead. An empty paragraph is finished immediately.
pub(crate) fn enqueue(
    text: String,
    spans: Vec<(Range<usize>, ProofLang)>,
    skip: Vec<Range<usize>>,
    spelling: Options,
    grammar: bool,
    list_item: bool,
    typing: Option<(usize, usize)>,
) -> Option<Arc<IssueMarks>> {
    let marks = Arc::new(IssueMarks::default());
    if text.trim().is_empty() || text.len() > 100_000 {
        return Some(marks);
    }
    let job = Job { marks: Arc::clone(&marks), text, spans, skip, spelling, grammar, list_item, typing };
    let tx = worker()?;
    PENDING.fetch_add(1, Ordering::Release);
    if tx.send(job).is_err() {
        PENDING.fetch_sub(1, Ordering::Release);
        return None;
    }
    Some(marks)
}

fn worker() -> Option<&'static mpsc::Sender<Job>> {
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        static TX: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
        Some(TX.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            let _ = thread::Builder::new().name("tavru-docs-proof".into()).spawn(move || {
                while let Ok(job) = rx.recv() {
                    let _finish = Finish;
                    run(job);
                }
            });
            tx
        }))
    }
}

struct Finish;

impl Drop for Finish {
    fn drop(&mut self) {
        PENDING.fetch_sub(1, Ordering::AcqRel);
    }
}

fn run(job: Job) {
    #[cfg(test)]
    hold_for_test();
    // The layout that asked was discarded (a measurement, or a newer layout replaced it).
    if Arc::strong_count(&job.marks) == 1 {
        return;
    }
    let skip = |a: usize, b: usize| job.skip.iter().any(|r| r.start < b && a < r.end);
    let issues: Vec<(usize, usize, bool)> = wordcraft_proof::check_text_with(&job.text, &job.spans, &job.spelling, job.grammar, job.list_item)
        .into_iter()
        .filter(|i| !skip(i.start, i.end))
        .map(|i| (i.start, i.end, i.kind == IssueKind::Grammar))
        .filter(|&(a, b, grammar)| grammar || !job.typing.is_some_and(|(s, e)| a < e && s < b))
        .collect();
    if Arc::strong_count(&job.marks) == 1 {
        return;
    }
    job.marks.set(issues);
}

#[cfg(test)]
struct Gate {
    go: Mutex<bool>,
    cv: std::sync::Condvar,
    arrived: AtomicUsize,
}

#[cfg(test)]
fn gate_slot() -> &'static Mutex<Option<Arc<Gate>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<Gate>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn hold_for_test() {
    let Some(gate) = gate_slot().lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
    gate.arrived.fetch_add(1, Ordering::Release);
    let mut go = gate.go.lock().unwrap_or_else(|e| e.into_inner());
    while !*go {
        go = gate.cv.wait(go).unwrap_or_else(|e| e.into_inner());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LayoutCache, LayoutOptions, layout};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use wordcraft_doc::Document;

    struct Release(Arc<Gate>);

    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
            *gate_slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    impl Gate {
        fn closed() -> Arc<Self> {
            Arc::new(Self { go: Mutex::new(false), cv: std::sync::Condvar::new(), arrived: AtomicUsize::new(0) })
        }
        fn open(&self) {
            *self.go.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.cv.notify_all();
        }
    }

    #[test]
    fn deferred_proofing_returns_before_the_dictionary_check() {
        let gate = Gate::closed();
        *gate_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&gate));
        let _release = Release(Arc::clone(&gate));
        let doc = Document::from_text("helo wrld");
        let opts = LayoutOptions { proofing: true, defer_proofing: true, ..LayoutOptions::default() };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(layout(&doc, &mut LayoutCache::new(), &opts));
        });
        let laid = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(laid) => laid,
            Err(_) => {
                assert!(false, "layout waited for the dictionary check");
                return;
            }
        };
        let slot = laid.pages[0].items.iter().find_map(|item| match item {
            crate::Placed::Lines { para, .. } => para.issue_slot.clone(),
            _ => None,
        });
        let Some(slot) = slot else {
            assert!(false, "the paragraph has no deferred marks");
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while gate.arrived.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(gate.arrived.load(Ordering::Acquire) > 0, "the check never left the layout thread");
        assert!(slot.snapshot().is_empty(), "marks were written before the checker was allowed to run");
        gate.open();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut marks = Vec::new();
        while Instant::now() < deadline {
            marks = slot.snapshot();
            if !marks.is_empty() {
                break;
            }
            thread::yield_now();
        }
        let words: Vec<&str> = marks.iter().filter(|m| !m.2).map(|m| &"helo wrld"[m.0..m.1]).collect();
        assert_eq!(words, ["helo", "wrld"]);
    }
}
