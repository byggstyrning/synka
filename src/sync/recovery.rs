//! Quiesce engine work before moving a recovery tree. A timeout must leave it untouched.

use super::*;

pub(super) struct RecoveryGuard(Arc<Engine>);

pub(super) struct CommandGuard(Arc<Engine>);

impl Drop for CommandGuard {
    fn drop(&mut self) {
        self.0.st().commands_running -= 1;
    }
}

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        self.0.st().recovering = false;
        self.0.publish();
        self.0.wake.notify_one();
        // Watcher events were deferred while paths changed. Reconcile their current
        // state instead of applying events naming the old tree to the rebuilt one.
        if self.0.ctx.any_signed_in() && !self.0.recovery_blocked() {
            self.0.on_local(vec![LocalChange::Rescan]);
        }
    }
}

impl Engine {
    pub(super) fn begin_command(self: &Arc<Self>, cmd: &TrayCmd) -> Option<CommandGuard> {
        let mut st = self.st();
        if (st.recovering || st.moving)
            && !matches!(
                cmd,
                TrayCmd::OpenFolder
                    | TrayCmd::OpenPath(_)
                    | TrayCmd::OpenLog
                    | TrayCmd::Hello
                    | TrayCmd::ExportDiagnostics
                    | TrayCmd::CheckUpdates
                    | TrayCmd::TestApsClientId { .. }
                    | TrayCmd::CancelApsClientIdTest { .. }
                    // Sign-in steps touch no files: they may run during a rebuild or a move.
                    | TrayCmd::SignInMfa { .. }
                    | TrayCmd::CancelSignIn { .. }
                    | TrayCmd::FreeUpSpace { .. }
                    | TrayCmd::InstallUpdate
                    | TrayCmd::Quit
            )
        {
            return None;
        }
        st.commands_running += 1;
        Some(CommandGuard(self.clone()))
    }

    /// Admission and recovery's pause flag share one lock, so a worker cannot
    /// appear between the idle check and the atomic folder move.
    pub(super) fn try_maintenance_guard(self: &Arc<Self>) -> Option<MaintenanceGuard> {
        let mut st = self.st();
        if st.recovering || st.moving {
            return None;
        }
        st.maintenance_running += 1;
        Some(MaintenanceGuard(self.clone()))
    }

    pub(super) fn recovery_blocked(&self) -> bool {
        let st = self.st();
        st.recovering || st.moving
    }

    pub(super) fn begin_recovery(self: &Arc<Self>) -> Option<RecoveryGuard> {
        let mut st = self.st();
        if st.recovering || st.moving {
            return None;
        }
        st.recovering = true;
        Some(RecoveryGuard(self.clone()))
    }

    /// The caller owns one maintenance guard. Drain all older workers, including
    /// listings waiting on the network, before changing paths or deleting rows.
    pub(super) async fn wait_for_recovery_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let idle = {
                let st = self.st();
                !st.outbox_running
                    && st.polling.is_empty()
                    && st.downloads.is_empty()
                    && st.maintenance_running <= 1
                    && st.commands_running == 0
            };
            if idle {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep_until((Instant::now() + Duration::from_millis(100)).min(deadline))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::tests::{fixture, row};

    #[tokio::test]
    async fn recovery_timeout_preserves_local_work_rows_and_jobs() {
        let f = fixture("recovery-busy");
        let path = f.dir.join("edited.rvt");
        std::fs::write(&path, b"unsynced local work").unwrap();
        let mut item = row(&f.p, "LIN1", Some("co.ROOT"), "edited.rvt", Kind::File);
        item.local_state = LocalState::Error;
        f.ctx.store.upsert_item(&item).unwrap();
        f.ctx
            .store
            .enqueue_at(f.p.pid, "upload", Some("LIN1"), "{}", 0)
            .unwrap();
        let engine = Engine::new(f.ctx.clone());
        let _own = engine.maintenance_guard();
        engine.st().outbox_running = true;
        let gate = engine.begin_recovery().unwrap();
        assert!(!engine.is_quiet(), "an update must not restart a recovery");
        assert!(
            engine.begin_recovery().is_none(),
            "only one recovery can move paths"
        );
        assert!(
            !engine
                .wait_for_recovery_idle(Duration::from_millis(20))
                .await
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"unsynced local work");
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "LIN1")
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Error
        );
        assert!(f.ctx.store.has_job(f.p.pid, "LIN1", &["upload"]).unwrap());
        drop(gate);
        assert!(!engine.recovery_blocked());
    }

    #[tokio::test]
    async fn recovery_drains_listings_and_blocks_new_poll_and_watch_work() {
        let f = fixture("recovery-drain");
        let engine = Engine::new(f.ctx.clone());
        let _own = engine.maintenance_guard();
        let prior_listing = engine.maintenance_guard();
        let gate = engine.begin_recovery().unwrap();
        engine.spawn_poll(f.p.pid);
        let path = f.dir.join("new.txt");
        std::fs::write(&path, b"local").unwrap();
        engine.on_local(vec![LocalChange::Changed(path)]);
        assert!(engine.st().polling.is_empty());
        assert_eq!(
            engine.st().maintenance_running,
            2,
            "no new mutation worker starts"
        );
        assert!(
            !engine
                .wait_for_recovery_idle(Duration::from_millis(20))
                .await
        );
        drop(prior_listing);
        assert!(
            engine
                .wait_for_recovery_idle(Duration::from_millis(20))
                .await
        );
        drop(gate);
    }

    #[tokio::test]
    async fn recovery_waits_for_admitted_commands_and_refuses_new_mutations() {
        let f = fixture("recovery-command");
        let engine = Engine::new(f.ctx.clone());
        let _own = engine.maintenance_guard();
        let old_command = engine.begin_command(&TrayCmd::RetryFailed).unwrap();
        let gate = engine.begin_recovery().unwrap();
        assert!(engine
            .begin_command(&TrayCmd::SignOut {
                provider: None,
                account: None
            })
            .is_none());
        assert!(engine.try_maintenance_guard().is_none());
        assert!(
            !engine
                .wait_for_recovery_idle(Duration::from_millis(20))
                .await
        );
        drop(old_command);
        assert!(
            engine
                .wait_for_recovery_idle(Duration::from_millis(20))
                .await
        );
        drop(gate);
    }
    #[tokio::test]
    async fn recovery_defers_due_auto_install_without_starting_worker() {
        let f = fixture("recovery-auto-install");
        f.ctx.settings.write().unwrap().auto_update = true;
        let engine = Engine::new(f.ctx.clone());
        let now = Instant::now();
        {
            let mut st = engine.st();
            st.pending_update = Some(crate::update::UpdateInfo {
                version: "99.0.0-private-test".into(),
                asset_url: "https://example.invalid/private-test.msix".into(),
            });
            st.auto_install_at = Some(now);
        }
        let gate = engine.begin_recovery().unwrap();
        engine.maybe_auto_install(now);
        {
            let st = engine.st();
            assert_eq!(st.auto_install_at, Some(now + AUTO_INSTALL_RETRY));
            assert_eq!(
                st.maintenance_running, 0,
                "a recovery gate must not admit an automatic installer"
            );
            assert!(!st.installing);
            assert!(
                st.pending_update.is_some(),
                "the deferred update is retained"
            );
        }
        drop(gate);
        assert!(!engine.recovery_blocked());
    }
    #[tokio::test]
    async fn recovery_rejects_new_free_up_before_acquiring_resources() {
        let f = fixture("recovery-free-up-admission");
        let engine = Engine::new(f.ctx.clone());
        let gate = engine.begin_recovery().unwrap();
        engine.free_up_space(Some(f.p.project_id.clone()));
        assert_eq!(engine.free_up_sem.available_permits(), 1);
        assert_eq!(engine.st().maintenance_running, 0);
        drop(gate);
        assert!(!engine.recovery_blocked());
    }

    #[tokio::test]
    async fn recovery_drains_free_up_admitted_before_its_first_poll() {
        let f = fixture("recovery-free-up-drain");
        let engine = Engine::new(f.ctx.clone());
        let _own = engine.maintenance_guard();
        // Admission counts immediately, before the asynchronous scan can first run.
        engine.free_up_space(Some(f.p.project_id.clone()));
        assert_eq!(engine.free_up_sem.available_permits(), 0);
        assert_eq!(engine.st().maintenance_running, 2);
        let gate = engine.begin_recovery().unwrap();
        assert!(!engine.wait_for_recovery_idle(Duration::ZERO).await);
        // This scan touches only the fixture's private plain directories; no
        // installed provider or user files are involved. Let the real worker finish.
        tokio::time::timeout(Duration::from_secs(5), async {
            while engine.st().maintenance_running > 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the private free-up scan did not drain");
        assert_eq!(engine.free_up_sem.available_permits(), 1);
        assert!(engine.wait_for_recovery_idle(Duration::ZERO).await);
        drop(gate);
        assert!(!engine.recovery_blocked());
    }
}
