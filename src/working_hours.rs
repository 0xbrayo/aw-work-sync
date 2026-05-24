use aw_models::Event;
use chrono::{DateTime, Duration, Utc};

/// Generous approximation of worked time: fills gaps shorter than max_break by extending
/// the preceding event's duration, then sums all durations.
/// Port of aw-transform's flood() + generous_approx() from the Python client.
pub fn generous_approx(events: &[Event], max_break: Duration) -> Duration {
    if events.is_empty() {
        return Duration::zero();
    }

    let mut sorted = events.to_vec();
    sorted.sort_by_key(|e| e.timestamp);

    // Compute the total duration using fold, skipping the first element since it's our base case.
    let (total, _) = sorted.iter().skip(1).fold(
        (sorted[0].duration, sorted[0].timestamp + sorted[0].duration),
        |(total, prev_end), e| {
            let gap = e.timestamp - prev_end;
            let gap_fill = if gap > Duration::zero() && gap < max_break {
                gap
            } else {
                Duration::zero()
            };
            (total + gap_fill + e.duration, e.timestamp + e.duration)
        },
    );

    total
}

/// Returns (first_event_timestamp, last_event_end_timestamp) for boundary stitching.
/// `last_event_end` = timestamp + duration of the chronologically last event.
pub fn event_boundaries(events: &[Event]) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    if events.is_empty() {
        return None;
    }
    let first = events.iter().min_by_key(|e| e.timestamp)?;
    let last = events.iter().max_by_key(|e| e.timestamp + e.duration)?;
    Some((first.timestamp, last.timestamp + last.duration))
}
