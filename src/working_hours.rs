use aw_models::Event;
use chrono::Duration;

/// Generous approximation of worked time: fills gaps shorter than max_break by extending
/// the preceding event's duration, then sums all durations.
/// Port of aw-transform's flood() + generous_approx() from the Python client.
pub fn generous_approx(events: &[Event], max_break: Duration) -> Duration {
    if events.is_empty() {
        return Duration::zero();
    }

    let mut sorted = events.to_vec();
    sorted.sort_by_key(|e| e.timestamp);

    let mut durations: Vec<Duration> = sorted.iter().map(|e| e.duration).collect();
    let mut prev_end = sorted[0].timestamp + sorted[0].duration;

    for i in 1..sorted.len() {
        let gap = sorted[i].timestamp - prev_end;
        if gap > Duration::zero() && gap < max_break {
            durations[i - 1] += gap;
        }
        prev_end = sorted[i].timestamp + sorted[i].duration;
    }

    durations.iter().copied().fold(Duration::zero(), |a, b| a + b)
}
