//! User run intent survives a temporary absence of all target devices.

#[derive(Default)]
pub struct RunIntent {
    requested: bool,
}

impl RunIntent {
    pub fn start(&mut self) {
        self.requested = true;
    }

    pub fn stop(&mut self) {
        self.requested = false;
    }

    pub fn requested(&self) -> bool {
        self.requested
    }

    pub fn should_resume(&self, source_available: bool, connected_targets: usize) -> bool {
        self.requested && source_available && connected_targets > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_target_removal_keeps_intent_until_replug() {
        let mut intent = RunIntent::default();
        intent.start();
        assert!(intent.should_resume(true, 1));
        assert!(!intent.should_resume(true, 0));
        assert!(intent.requested());
        assert!(intent.should_resume(true, 1));
    }

    #[test]
    fn explicit_stop_while_waiting_prevents_later_replug_start() {
        let mut intent = RunIntent::default();
        intent.start();
        assert!(!intent.should_resume(true, 0));
        intent.stop();
        assert!(!intent.should_resume(true, 1));
    }

    #[test]
    fn missing_source_never_resumes_and_source_failure_can_clear_intent() {
        let mut intent = RunIntent::default();
        intent.start();
        assert!(!intent.should_resume(false, 2));
        intent.stop();
        assert!(!intent.should_resume(true, 2));
    }
}
