use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Overload;

pub const INCIDENT_GAP: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Episode {
    pub started_at: u64,
    pub sent: u32,
    pub last_sent_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Wait { episode: Episode, wait: Duration },
    GiveUp { episode: Episode },
}

pub fn next_step(
    settings: &Overload,
    previous: Option<Episode>,
    progressed: bool,
    now: u64,
    unit: f64,
) -> Step {
    let fresh = Episode {
        started_at: now,
        sent: 0,
        last_sent_at: None,
    };
    let episode = match previous {
        Some(episode) if !progressed && !is_stale(&episode, now) => episode,
        _ => fresh,
    };

    let cap = u64::from(settings.max_total_minutes) * 60;
    if now.saturating_sub(episode.started_at) >= cap {
        return Step::GiveUp { episode };
    }

    Step::Wait {
        episode,
        wait: wait(settings, episode.sent, unit),
    }
}

pub fn wait(settings: &Overload, level: u32, unit: f64) -> Duration {
    let base = usize::try_from(level)
        .ok()
        .and_then(|index| settings.backoff_seconds.get(index))
        .copied()
        .unwrap_or(settings.steady_seconds);
    let spread = f64::from(settings.jitter_percent) / 100.0;
    let factor = 1.0 + (unit * 2.0 - 1.0) * spread;

    Duration::from_secs_f64((f64::from(base) * factor).max(0.0))
}

pub fn jitter_unit() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);

    f64::from(nanos) / 1e9
}

fn is_stale(episode: &Episode, now: u64) -> bool {
    let last = episode.last_sent_at.unwrap_or(episode.started_at);

    now.saturating_sub(last) > INCIDENT_GAP.as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn waited(step: Step) -> (Episode, u64) {
        match step {
            Step::Wait { episode, wait } => (episode, wait.as_secs()),
            Step::GiveUp { .. } => panic!("gave up"),
        }
    }

    #[test]
    fn ladder_climbs_then_holds_steady_with_bounded_jitter() {
        let settings = Overload::default();
        let waits: Vec<u64> = (0..7)
            .map(|level| wait(&settings, level, 0.5).as_secs())
            .collect();
        assert_eq!(waits, vec![30, 60, 120, 240, 300, 300, 300]);
        assert_eq!(wait(&settings, 0, 0.0).as_secs_f64(), 25.5);
        assert_eq!(wait(&settings, 0, 1.0).as_secs_f64(), 34.5);
    }

    #[test]
    fn an_episode_continues_until_progress_or_a_long_gap() {
        let settings = Overload::default();
        let running = Episode {
            started_at: NOW - 100,
            sent: 2,
            last_sent_at: Some(NOW - 10),
        };

        let (episode, wait) = waited(next_step(&settings, Some(running), false, NOW, 0.5));
        assert_eq!((episode, wait), (running, 120));

        let (episode, wait) = waited(next_step(&settings, Some(running), true, NOW, 0.5));
        assert_eq!((episode.started_at, episode.sent, wait), (NOW, 0, 30));

        let later = NOW + INCIDENT_GAP.as_secs() + 1;
        let (episode, _) = waited(next_step(&settings, Some(running), false, later, 0.5));
        assert_eq!((episode.started_at, episode.sent), (later, 0));

        let (episode, wait) = waited(next_step(&settings, None, false, NOW, 0.5));
        assert_eq!((episode.started_at, episode.sent, wait), (NOW, 0, 30));
    }

    #[test]
    fn the_total_cap_ends_the_episode() {
        let settings = Overload {
            max_total_minutes: 1,
            ..Overload::default()
        };
        let running = Episode {
            started_at: NOW - 60,
            sent: 1,
            last_sent_at: Some(NOW - 5),
        };

        assert_eq!(
            next_step(&settings, Some(running), false, NOW, 0.5),
            Step::GiveUp { episode: running }
        );
    }
}
