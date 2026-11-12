//! Per-folder poll tiers, for StreamBIM.
//!
//! StreamBIM asks clients not to poll hard (docs/specs/streambim-endpoints.md §10.1), and each
//! folder costs at least two requests (its subfolders, then its documents). So between full walks
//! a StreamBIM poll lists only the folders where something happened in the last ACTIVE_WINDOW:
//!
//! - the user opened the folder in Explorer, or opened, saved, renamed or deleted something in it;
//! - a poll found a remote change in it.
//!
//! Every populated folder is still listed at least every FULL_EVERY (the default tier's 300 s),
//! after "Sync now" and on the first poll after start. When a poll finds that an item has left an
//! active folder, it lists the quiet folders too before it applies anything. The item may have
//! moved into one of them, and only a full listing keeps that a move. Otherwise it would be
//! deleted locally and come back as a new item at the next full walk.
//!
//! The project's own tier (`interval_secs`) still decides when it polls.

use super::ACTIVE_WINDOW;
use crate::model::{Pid, Provider};
use crate::store::Store;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::time::Instant;

/// Every populated folder is listed at least this often.
pub(crate) const FULL_EVERY: Duration = Duration::from_secs(300);
/// A poll can come up to 10 % early (jitter). A full walk this close to due is done now, so a
/// project on the 300 s tier lists everything on every poll.
const EARLY: Duration = Duration::from_secs(30);
/// Spots kept per project before the stale ones are dropped.
const PRUNE_AT: usize = 64;

/// True for the providers whose polls list only active folders between full walks.
pub(crate) fn per_folder(provider: Provider) -> bool {
    provider == Provider::StreamBim
}

/// Where something happened in a project.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Spot {
    /// In this folder: it was opened, or its listing changed.
    Folder(String),
    /// To this item, so in the folder that holds it: a file opened, an item renamed or deleted.
    In(String),
}

#[derive(Default)]
pub(crate) struct Hot {
    spots: HashMap<Pid, HashMap<Spot, Instant>>,
    /// When a walk last listed every populated folder of the project.
    full_at: HashMap<Pid, Instant>,
}

impl Hot {
    pub fn touch(&mut self, pid: Pid, spot: Spot, now: Instant) {
        let spots = self.spots.entry(pid).or_default();
        spots.insert(spot, now);
        if spots.len() > PRUNE_AT {
            spots.retain(|_, at| now.saturating_duration_since(*at) < ACTIVE_WINDOW);
        }
    }

    /// The spots of `pid` active within ACTIVE_WINDOW. Older ones are dropped.
    pub fn active(&mut self, pid: Pid, now: Instant) -> Vec<Spot> {
        let Some(spots) = self.spots.get_mut(&pid) else {
            return Vec::new();
        };
        spots.retain(|_, at| now.saturating_duration_since(*at) < ACTIVE_WINDOW);
        let out = spots.keys().cloned().collect();
        if spots.is_empty() {
            self.spots.remove(&pid);
        }
        out
    }

    /// True when the next poll of `pid` must list every populated folder.
    pub fn full_due(&self, pid: Pid, now: Instant) -> bool {
        self.full_at
            .get(&pid)
            .is_none_or(|at| now.saturating_duration_since(*at) + EARLY >= FULL_EVERY)
    }

    /// A walk that started at `at` listed every populated folder of `pid`.
    pub fn walked_all(&mut self, pid: Pid, at: Instant) {
        self.full_at.insert(pid, at);
    }

    /// The next poll of `pid` lists every populated folder ("Sync now").
    pub fn want_full(&mut self, pid: Pid) {
        self.full_at.remove(&pid);
    }

    pub fn forget(&mut self, pid: Pid) {
        self.spots.remove(&pid);
        self.full_at.remove(&pid);
    }
}

/// The folder ids that `spots` point at. An item that is gone points nowhere.
pub(crate) fn folders(store: &Store, pid: Pid, spots: Vec<Spot>) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for spot in spots {
        match spot {
            Spot::Folder(id) => {
                out.insert(id);
            }
            Spot::In(id) => {
                if let Some(parent) = store.item(pid, &id)?.and_then(|it| it.parent) {
                    out.insert(parent);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(id: &str) -> Spot {
        Spot::Folder(id.into())
    }

    #[test]
    fn spots_stay_active_for_the_active_window() {
        let mut hot = Hot::default();
        let t0 = Instant::now();
        hot.touch(1, f("7"), t0);
        hot.touch(1, Spot::In("d12".into()), t0 + Duration::from_secs(300));
        hot.touch(2, f("9"), t0);
        let mut at_9_min = hot.active(1, t0 + Duration::from_secs(9 * 60));
        at_9_min.sort_by_key(|s| format!("{s:?}"));
        assert_eq!(at_9_min, [f("7"), Spot::In("d12".into())]);
        assert_eq!(
            hot.active(1, t0 + ACTIVE_WINDOW),
            [Spot::In("d12".into())],
            "the folder opened ten minutes ago is quiet"
        );
        assert!(hot.active(1, t0 + 2 * ACTIVE_WINDOW).is_empty());
        assert!(
            !hot.spots.contains_key(&1),
            "a project with no spots is dropped"
        );
        assert_eq!(hot.active(2, t0), [f("9")], "projects are kept apart");
    }

    #[test]
    fn many_spots_drop_the_stale_ones() {
        let mut hot = Hot::default();
        let t0 = Instant::now();
        for i in 0..PRUNE_AT {
            hot.touch(1, f(&i.to_string()), t0);
        }
        hot.touch(1, f("new"), t0 + ACTIVE_WINDOW);
        assert_eq!(hot.spots[&1].len(), 1);
    }

    #[test]
    fn a_full_walk_is_due_every_five_minutes_and_after_sync_now() {
        let mut hot = Hot::default();
        let t0 = Instant::now();
        assert!(hot.full_due(1, t0), "the first poll lists everything");
        hot.walked_all(1, t0);
        for s in [0, 30, 120, 269] {
            assert!(!hot.full_due(1, t0 + Duration::from_secs(s)), "{s} s");
        }
        // The 300 s tier comes up to 30 s early and still lists everything.
        assert!(hot.full_due(1, t0 + Duration::from_secs(270)));
        assert!(hot.full_due(1, t0 + FULL_EVERY));
        assert!(hot.full_due(2, t0), "another project has its own walk");
        hot.want_full(1);
        assert!(hot.full_due(1, t0));
        hot.walked_all(1, t0);
        hot.touch(1, f("7"), t0);
        hot.forget(1);
        assert!(hot.full_due(1, t0));
        assert!(hot.active(1, t0).is_empty());
    }

    #[test]
    fn only_streambim_has_per_folder_tiers() {
        assert!(per_folder(Provider::StreamBim));
        assert!(!per_folder(Provider::Acc));
        assert!(!per_folder(Provider::Interaxo));
    }
}
