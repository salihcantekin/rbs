//! What a browser source's page reports: whether it runs and on which path.

use rbs_scene::model::SourceId;

use super::Studio;

impl Studio {
    /// True while browser source `source` has a running page (a helper process).
    pub fn browser_running(&self, source: SourceId) -> bool {
        self.browser_controls
            .get(&source)
            .is_some_and(|control| control.helper_running())
    }

    /// True while browser source `source` draws on shared memory because its GPU path
    /// failed, until the source is stopped (ADR-0157 decision 9). Never waits.
    pub fn browser_gpu_fell_back(&self, source: SourceId) -> bool {
        self.browser_controls
            .get(&source)
            .is_some_and(|control| control.gpu_fell_back())
    }
}
