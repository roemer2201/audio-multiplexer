//! User run intent survives a temporary absence of all target devices.

#[derive(Default)]
pub struct RunIntent {
    requested: bool,
}

/// A target-set change must never remove an unchanged healthy output.
pub struct TargetChanges {
    pub remove: Vec<String>,
    pub add: Vec<String>,
}

impl TargetChanges {
    pub fn between(active: &[String], desired: &[String]) -> Self {
        Self {
            remove: active
                .iter()
                .filter(|id| !desired.contains(id))
                .cloned()
                .collect(),
            add: desired
                .iter()
                .filter(|id| !active.contains(id))
                .cloned()
                .collect(),
        }
    }
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
    fn removing_and_rejoining_b_leaves_a_untouched() {
        let a = "a".to_string();
        let b = "b".to_string();
        let removal = TargetChanges::between(&[a.clone(), b.clone()], std::slice::from_ref(&a));
        assert_eq!(removal.remove, vec![b.clone()]);
        assert!(removal.add.is_empty());
        let rejoin = TargetChanges::between(std::slice::from_ref(&a), &[a.clone(), b.clone()]);
        assert_eq!(rejoin.add, vec![b]);
        assert!(rejoin.remove.is_empty());
        let reordered = TargetChanges::between(&[a.clone(), "b".into()], &["b".into(), a]);
        assert!(reordered.add.is_empty());
        assert!(reordered.remove.is_empty());
    }

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
