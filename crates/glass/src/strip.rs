//! The health strip: thirty days of an app's status, one mark an hour.
//!
//! Each mark is the worst status the app held at any moment of that hour, so
//! a five-minute outage at 3am is still visible a month later. Before the
//! app existed there is no mark.

use canon::Status;

pub const HOURS: usize = 720;
pub const HOUR_MS: i64 = 3_600_000;

/// How bad a status is to have held, for choosing an hour's mark.
fn weight(s: Status) -> u8 {
    match s {
        Status::Live => 0,
        Status::Stopped => 1,
        Status::Onboarding => 2,
        Status::NeedsYou => 3,
        Status::Degraded => 4,
        Status::Stuck => 5,
        Status::Down => 6,
    }
}

/// When each status began: `Onboarding` at `created_at`, then every recorded
/// change after it. `history` is [`canon::Canon::status_history`]'s answer.
fn changes(created_at: i64, history: &[(i64, Status)]) -> Vec<(i64, Status)> {
    // A change recorded before this app was created belongs to an earlier
    // app of the same name.
    let mut out = vec![(created_at, Status::Onboarding)];
    out.extend(history.iter().copied().filter(|&(at, _)| at >= created_at));
    out
}

/// The marks for the `HOURS` hours ending at `now`, oldest first.
pub fn marks(now: i64, created_at: i64, history: &[(i64, Status)]) -> Vec<Option<Status>> {
    let changes = changes(created_at, history);
    let start = now - HOURS as i64 * HOUR_MS;
    let mut next = 0;
    let mut current: Option<Status> = None;
    (0..HOURS as i64)
        .map(|h| {
            let from = start + h * HOUR_MS;
            // The last hour holds `now` itself: a change made this very
            // millisecond is marked.
            let to = if h + 1 == HOURS as i64 { now + 1 } else { from + HOUR_MS };
            while next < changes.len() && changes[next].0 <= from {
                current = Some(changes[next].1);
                next += 1;
            }
            let mut worst = current;
            while next < changes.len() && changes[next].0 < to {
                let s = changes[next].1;
                if worst.is_none_or(|w| weight(s) > weight(w)) {
                    worst = Some(s);
                }
                current = Some(s);
                next += 1;
            }
            worst
        })
        .collect()
}

/// One character a mark, for the wire: `.` for no app yet.
pub fn encode(marks: &[Option<Status>]) -> String {
    marks
        .iter()
        .map(|m| match m {
            None => '.',
            Some(Status::Live) => 'l',
            Some(Status::Stopped) => 's',
            Some(Status::Onboarding) => 'o',
            Some(Status::NeedsYou) => 'n',
            Some(Status::Degraded) => 'd',
            Some(Status::Stuck) => 'k',
            Some(Status::Down) => 'x',
        })
        .collect()
}

/// The share of the window the app was live, out of the time it was meant
/// to be serving: hours stopped on purpose, and everything before it first
/// went live, count for nothing either way. `None` until it has been live.
pub fn live_share(now: i64, created_at: i64, history: &[(i64, Status)]) -> Option<f64> {
    let changes = changes(created_at, history);
    let first_live = changes.iter().position(|&(_, s)| s == Status::Live)?;
    let changes = &changes[first_live..];
    let start = now - HOURS as i64 * HOUR_MS;
    let (mut live, mut serving) = (0i64, 0i64);
    for (i, &(at, s)) in changes.iter().enumerate() {
        let end = changes.get(i + 1).map_or(now, |c| c.0).min(now);
        let begin = at.max(start);
        if end <= begin || matches!(s, Status::Stopped | Status::Onboarding) {
            continue;
        }
        serving += end - begin;
        if s == Status::Live {
            live += end - begin;
        }
    }
    (serving > 0).then(|| live as f64 / serving as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000 * HOUR_MS;

    fn at(hours_ago: f64) -> i64 {
        NOW - (hours_ago * HOUR_MS as f64) as i64
    }

    #[test]
    fn an_app_made_this_very_millisecond_has_a_mark() {
        let m = marks(NOW, NOW, &[(NOW, Status::Live)]);
        assert_eq!(m[HOURS - 1], Some(Status::Onboarding), "the worst it held this hour");
        assert_eq!(m[HOURS - 2], None);
    }

    #[test]
    fn a_short_outage_marks_its_whole_hour() {
        let history = [(at(800.0), Status::Live), (at(10.5), Status::Down), (at(10.4), Status::Live)];
        let m = marks(NOW, at(900.0), &history);
        assert_eq!(m.len(), HOURS);
        assert_eq!(m[HOURS - 11], Some(Status::Down), "the hour from 11h to 10h ago");
        assert_eq!(m[HOURS - 10], Some(Status::Live));
        assert_eq!(m[HOURS - 12], Some(Status::Live));
        assert_eq!(encode(&m).matches('x').count(), 1);
    }

    #[test]
    fn hours_before_the_app_existed_have_no_mark() {
        let m = marks(NOW, at(2.5), &[(at(2.0), Status::Live)]);
        assert!(m[..HOURS - 3].iter().all(Option::is_none));
        assert_eq!(m[HOURS - 3], Some(Status::Onboarding));
        assert_eq!(m[HOURS - 2], Some(Status::Live));
        assert_eq!(&encode(&m)[HOURS - 4..], ".oll");
    }

    #[test]
    fn the_status_held_before_the_window_carries_in() {
        let m = marks(NOW, at(2000.0), &[(at(1500.0), Status::Degraded)]);
        assert!(m.iter().all(|s| *s == Some(Status::Degraded)));
    }

    #[test]
    fn an_earlier_app_of_the_same_name_leaves_no_mark() {
        let m = marks(NOW, at(5.0), &[(at(50.0), Status::Down)]);
        assert_eq!(m[HOURS - 5], Some(Status::Onboarding));
        assert!(m[..HOURS - 5].iter().all(Option::is_none));
    }

    #[test]
    fn live_share_ignores_time_stopped_or_onboarding() {
        let history = [
            (at(100.0), Status::Live),
            (at(50.0), Status::Down),
            (at(40.0), Status::Live),
            (at(20.0), Status::Stopped),
        ];
        let share = live_share(NOW, at(110.0), &history).unwrap();
        // 70h live out of 80h serving.
        assert!((share - 70.0 / 80.0).abs() < 1e-9, "{share}");
        assert_eq!(live_share(NOW, at(3.0), &[]), None);
        assert_eq!(live_share(NOW, at(3.0), &[(at(2.0), Status::NeedsYou)]), None, "never live yet");
    }
}
