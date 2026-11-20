//! A browser page whose GPU path fails starts again on shared memory (ADR-0157
//! decision 9): no back-off, the slot stays held, the choice stays until the source
//! is stopped. No CEF.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rbs_browser_ipc::Configure;
use rbs_scene::model::SourceId;
use rbs_sources::{
    BrowserBudget, BrowserClaim, BrowserFrameSink, BrowserSession, BrowserSettings, BrowserSound,
    BrowserSourceNode, BrowserTree, ShowClass,
};

/// Counts starts on each path. A GPU tree ends with a GPU failure once `fail` is set.
#[derive(Default)]
struct Tree {
    gpu_starts: AtomicU32,
    shm_starts: AtomicU32,
    fail: Arc<AtomicBool>,
}

struct Session {
    /// Set: this GPU tree's path failed and it ended.
    fail: Option<Arc<AtomicBool>>,
    stopped: bool,
}

impl BrowserSession for Session {
    fn poll(&mut self) -> Option<i32> {
        let failed = self
            .fail
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed));
        (failed || self.stopped).then_some(9)
    }
    fn stop(&mut self) {
        self.stopped = true;
    }
    fn gpu_fallback(&self) -> bool {
        self.fail
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    }
}

impl BrowserTree for Tree {
    fn start(
        &self,
        _config: &Configure,
        _sink: Arc<BrowserFrameSink>,
        _sound: Option<Arc<BrowserSound>>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        self.gpu_starts.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(Session {
            fail: Some(Arc::clone(&self.fail)),
            stopped: false,
        }))
    }

    fn start_on_shared_memory(
        &self,
        _config: &Configure,
        _sink: Arc<BrowserFrameSink>,
        _sound: Option<Arc<BrowserSound>>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        self.shm_starts.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(Session {
            fail: None,
            stopped: false,
        }))
    }
}

fn node(tree: &Arc<Tree>, budget: &Arc<BrowserBudget>, source: SourceId) -> BrowserSourceNode {
    budget.grant(&[BrowserClaim {
        source,
        class: ShowClass::Program,
    }]);
    BrowserSourceNode::new(
        BrowserSettings::page("https://example.com/"),
        Arc::clone(budget),
        source,
        Some(Arc::clone(tree) as Arc<dyn BrowserTree>),
    )
}

#[test]
fn a_gpu_failure_while_running_restarts_on_shared_memory_without_back_off() {
    let tree = Arc::new(Tree::default());
    let budget = BrowserBudget::shared();
    let source = SourceId::from_raw(1);
    let mut node = node(&tree, &budget, source);
    node.activate().unwrap();
    assert_eq!(tree.gpu_starts.load(Ordering::Relaxed), 1);

    tree.fail.store(true, Ordering::Relaxed);
    node.poll();
    assert_eq!(tree.shm_starts.load(Ordering::Relaxed), 1, "started again");
    assert!(node.helper_running());
    assert!(budget.holds(source), "the slot stays held");
    assert_eq!(node.attempts(), 0, "no back-off");
    assert_eq!(node.reason(), None);
    assert!(node.control().gpu_fell_back());

    // A later poll keeps the shared-memory tree; it reports no GPU failure.
    node.poll();
    assert_eq!(tree.shm_starts.load(Ordering::Relaxed), 1);
    assert_eq!(tree.gpu_starts.load(Ordering::Relaxed), 1);
}

#[test]
fn a_gpu_failure_at_start_still_starts_the_page_and_a_stop_clears_the_choice() {
    let tree = Arc::new(Tree::default());
    tree.fail.store(true, Ordering::Relaxed);
    let budget = BrowserBudget::shared();
    let source = SourceId::from_raw(2);
    let mut node = node(&tree, &budget, source);
    node.activate().unwrap();
    assert_eq!(tree.gpu_starts.load(Ordering::Relaxed), 1);
    assert_eq!(tree.shm_starts.load(Ordering::Relaxed), 1);
    assert!(node.helper_running());
    assert!(node.control().gpu_fell_back());

    // Stopping the source gives the next start the GPU path again.
    node.standby();
    assert!(!node.control().gpu_fell_back());
    tree.fail.store(false, Ordering::Relaxed);
    budget.grant(&[BrowserClaim {
        source,
        class: ShowClass::Program,
    }]);
    node.activate().unwrap();
    assert_eq!(tree.gpu_starts.load(Ordering::Relaxed), 2);
    assert_eq!(tree.shm_starts.load(Ordering::Relaxed), 1);
}
