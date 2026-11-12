//! Provider-wide pause after HTTP 429 (too many requests).
//!
//! Autodesk counts its limits per APS app (client ID) and endpoint, for every install of that
//! app together (300 folder listings a minute, 50 new items a minute, ...). StreamBIM publishes
//! no numbers but may block a client that polls too hard. `http` already honours Retry-After
//! (capped at 60 s) on each request. A 429 that is still there after those retries pauses the
//! whole provider here:
//!
//! - its projects do not poll, and a running folder walk stops before its next folder;
//! - its queued jobs wait without spending an attempt (as when offline).
//!
//! The first pause is 60 s, at least the capped Retry-After the request has already waited out.
//! Each further 429 after a pause doubles it, up to 15 min. A good poll after the pause starts
//! over at 60 s. A 429 during a pause leaves it as it is: requests that were already in flight
//! answer together and must not lengthen it. Files the user opens are still fetched.

use crate::http::ApiError;
use crate::model::Provider;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tracing::warn;

const FIRST_PAUSE: Duration = Duration::from_secs(60);
const MAX_PAUSE: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub struct Throttle(Mutex<HashMap<Provider, Pause>>);

#[derive(Clone, Copy)]
struct Pause {
    until: Instant,
    /// 429 answers that started a pause since the last good poll.
    strikes: u32,
}

/// The work stopped before sending a request because its provider is paused.
#[derive(Debug, thiserror::Error)]
#[error("{} begränsar antalet anrop; synken väntar", .0.label())]
pub struct Paused(pub Provider);

impl Throttle {
    fn map(&self) -> MutexGuard<'_, HashMap<Provider, Pause>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a 429 that `provider` still answered after the request's own retries. Starts a
    /// pause unless one is running.
    pub fn hit(&self, provider: Provider) {
        if let Some(len) = self.hit_at(provider, Instant::now()) {
            warn!(
                "{}: för många anrop (HTTP 429), synken pausas i {} s",
                provider.label(),
                len.as_secs()
            );
        }
    }

    /// The pause that the 429 started, or None when `provider` was already paused.
    fn hit_at(&self, provider: Provider, now: Instant) -> Option<Duration> {
        let mut map = self.map();
        let pause = map.entry(provider).or_insert(Pause {
            until: now,
            strikes: 0,
        });
        if pause.until > now {
            return None;
        }
        pause.strikes = pause.strikes.saturating_add(1);
        let len = pause_len(pause.strikes);
        pause.until = now + len;
        Some(len)
    }

    /// What is left of `provider`'s pause; None when it is not paused.
    pub fn remaining(&self, provider: Provider) -> Option<Duration> {
        self.remaining_at(provider, Instant::now())
    }

    fn remaining_at(&self, provider: Provider, now: Instant) -> Option<Duration> {
        self.map()
            .get(&provider)
            .map(|p| p.until.saturating_duration_since(now))
            .filter(|left| !left.is_zero())
    }

    /// A poll of `provider` went through: the next 429 starts again at the first pause. A running
    /// pause is kept (a poll that started before it may finish during it).
    pub fn ok(&self, provider: Provider) {
        self.ok_at(provider, Instant::now());
    }

    fn ok_at(&self, provider: Provider, now: Instant) {
        let mut map = self.map();
        if map.get(&provider).is_some_and(|p| p.until <= now) {
            map.remove(&provider);
        }
    }

    /// Every paused provider with what is left of its pause, in `Provider::ALL` order.
    pub fn paused(&self) -> Vec<(Provider, Duration)> {
        let now = Instant::now();
        Provider::ALL
            .into_iter()
            .filter_map(|p| self.remaining_at(p, now).map(|left| (p, left)))
            .collect()
    }
}

/// 60 s, doubled for each further strike, at most 15 min.
fn pause_len(strikes: u32) -> Duration {
    let doublings = strikes.saturating_sub(1).min(10);
    FIRST_PAUSE.saturating_mul(1 << doublings).min(MAX_PAUSE)
}

/// True when the provider answered 429 anywhere in `e`'s chain.
pub fn is_429(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<ApiError>()
            .is_some_and(|a| a.status() == Some(429))
    })
}

/// True when the work stopped because its provider was paused (`Paused`).
pub fn is_paused(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Paused>().is_some())
}

/// Swedish status line while signed-in providers are paused, for example "ACC begränsar antalet
/// anrop. Försöker igen om 2 min". None when none of `signed` is paused.
pub fn status_text(paused: &[(Provider, Duration)], signed: &[Provider]) -> Option<String> {
    let hit: Vec<(Provider, Duration)> = paused
        .iter()
        .copied()
        .filter(|(p, _)| signed.contains(p))
        .collect();
    let left = hit.iter().map(|(_, left)| *left).min()?;
    let who = hit
        .iter()
        .map(|(p, _)| p.label())
        .collect::<Vec<_>>()
        .join(" och ");
    Some(format!(
        "{who} begränsar antalet anrop. Försöker igen om {}",
        wait_text(left)
    ))
}

/// "45 s" under two minutes, else whole minutes rounded up ("3 min").
pub fn wait_text(left: Duration) -> String {
    let secs = left.as_secs().max(1);
    if secs < 120 {
        format!("{secs} s")
    } else {
        format!("{} min", secs.div_ceil(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(status: u16) -> anyhow::Error {
        ApiError::Status {
            status,
            body: "{}".into(),
        }
        .into()
    }

    #[test]
    fn pauses_double_after_each_pause_up_to_fifteen_minutes() {
        let t = Throttle::default();
        let mut now = Instant::now();
        let mut lens = Vec::new();
        for _ in 0..7 {
            let len = t.hit_at(Provider::Acc, now).unwrap();
            assert_eq!(t.remaining_at(Provider::Acc, now), Some(len));
            lens.push(len.as_secs());
            now += len;
            assert_eq!(t.remaining_at(Provider::Acc, now), None, "the pause ended");
        }
        assert_eq!(lens, [60, 120, 240, 480, 900, 900, 900]);
    }

    #[test]
    fn a_429_during_a_pause_does_not_lengthen_it() {
        let t = Throttle::default();
        let now = Instant::now();
        assert_eq!(t.hit_at(Provider::Acc, now), Some(FIRST_PAUSE));
        for s in [0, 1, 30, 59] {
            assert_eq!(t.hit_at(Provider::Acc, now + Duration::from_secs(s)), None);
        }
        assert_eq!(
            t.remaining_at(Provider::Acc, now + Duration::from_secs(30)),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn a_good_poll_after_the_pause_starts_over_but_never_ends_a_pause() {
        let t = Throttle::default();
        let now = Instant::now();
        t.hit_at(Provider::Acc, now);
        t.ok_at(Provider::Acc, now + Duration::from_secs(10));
        assert!(
            t.remaining_at(Provider::Acc, now + Duration::from_secs(10))
                .is_some(),
            "a poll that started before the pause keeps it"
        );
        let after = now + FIRST_PAUSE;
        assert_eq!(t.hit_at(Provider::Acc, after), Some(2 * FIRST_PAUSE));
        let after = after + 2 * FIRST_PAUSE;
        t.ok_at(Provider::Acc, after);
        assert_eq!(t.hit_at(Provider::Acc, after), Some(FIRST_PAUSE));
    }

    #[test]
    fn providers_pause_separately() {
        let t = Throttle::default();
        let now = Instant::now();
        t.hit_at(Provider::StreamBim, now);
        assert!(t.remaining_at(Provider::StreamBim, now).is_some());
        assert!(t.remaining_at(Provider::Acc, now).is_none());
        assert!(t.remaining_at(Provider::Interaxo, now).is_none());
        t.hit(Provider::Acc);
        let paused: Vec<Provider> = t.paused().into_iter().map(|(p, _)| p).collect();
        assert_eq!(paused, [Provider::Acc, Provider::StreamBim]);
    }

    #[test]
    fn only_429_answers_and_pauses_are_recognised() {
        assert!(is_429(&st(429)));
        assert!(is_429(&st(429).context("lista mappen")));
        for status in [400, 403, 408, 500, 503] {
            assert!(!is_429(&st(status)), "{status}");
        }
        assert!(!is_429(&anyhow::anyhow!("HTTP 429")));
        assert!(!is_paused(&st(429)));
        let paused: anyhow::Error = Paused(Provider::Acc).into();
        assert!(is_paused(&paused));
        assert!(is_paused(&paused.context("poll")));
        assert_eq!(
            Paused(Provider::StreamBim).to_string(),
            "StreamBIM begränsar antalet anrop; synken väntar"
        );
    }

    #[test]
    fn status_names_the_signed_in_paused_providers() {
        let paused = [
            (Provider::Acc, Duration::from_secs(150)),
            (Provider::StreamBim, Duration::from_secs(45)),
        ];
        assert_eq!(status_text(&paused, &[]), None);
        assert_eq!(status_text(&paused, &[Provider::Interaxo]), None);
        assert_eq!(
            status_text(&paused, &[Provider::Acc]).unwrap(),
            "ACC begränsar antalet anrop. Försöker igen om 3 min"
        );
        assert_eq!(
            status_text(&paused, &[Provider::Acc, Provider::StreamBim]).unwrap(),
            "ACC och StreamBIM begränsar antalet anrop. Försöker igen om 45 s"
        );
        assert_eq!(wait_text(Duration::ZERO), "1 s");
        assert_eq!(wait_text(Duration::from_secs(119)), "119 s");
        assert_eq!(wait_text(Duration::from_secs(120)), "2 min");
        assert_eq!(wait_text(Duration::from_secs(121)), "3 min");
    }
}
