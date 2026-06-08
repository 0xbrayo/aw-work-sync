use aw_models::Event;
use chrono::{DateTime, Duration, Utc};

/// Generous approximation of worked time: fills gaps shorter than max_break by extending
/// the preceding event's duration, then sums all durations.
/// Port of aw-transform's flood() + generous_approx() from the Python client.
pub fn generous_approx(events: &[Event], max_break: Duration) -> Duration {
    if events.is_empty() {
        return Duration::zero();
    }

    // Only timestamp + duration are needed; collect those (both Copy) instead of
    // cloning whole Events, whose `data` JSON map is the bulk of their size.
    let mut spans: Vec<(DateTime<Utc>, Duration)> =
        events.iter().map(|e| (e.timestamp, e.duration)).collect();
    spans.sort_by_key(|(ts, _)| *ts);

    // Compute the total duration using fold, skipping the first element since it's our base case.
    let (total, _) = spans.iter().skip(1).fold(
        (spans[0].1, spans[0].0 + spans[0].1),
        |(total, prev_end), &(ts, dur)| {
            let gap = ts - prev_end;
            let gap_fill = if gap > Duration::zero() && gap < max_break {
                gap
            } else {
                Duration::zero()
            };
            (total + gap_fill + dur, ts + dur)
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
