use chrono::{DateTime, Duration, Utc};
use janitor_publish::calculate_next_try_time;

#[test]
fn zero_attempts_returns_immediate() {
    let finish_time = Utc::now();
    assert_eq!(calculate_next_try_time(finish_time, 0), finish_time);
}

#[test]
fn exponential_backoff_progression() {
    let finish_time = DateTime::parse_from_rfc3339("2023-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    for (attempts, expected_hours) in [(1, 2), (2, 4), (3, 8), (4, 16), (5, 32), (6, 64), (7, 128)]
    {
        let result = calculate_next_try_time(finish_time, attempts);
        let expected = finish_time + Duration::hours(expected_hours);
        assert_eq!(
            result, expected,
            "attempt_count={} expected {}h",
            attempts, expected_hours
        );
    }
}

#[test]
fn caps_at_seven_days() {
    let finish_time = DateTime::parse_from_rfc3339("2023-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    for attempts in [8, 10, 15, 20, 100] {
        let result = calculate_next_try_time(finish_time, attempts);
        assert_eq!(result, finish_time + Duration::days(7));
    }
}

#[test]
fn cap_transition_between_seven_and_eight_attempts() {
    let finish_time = Utc::now();
    assert_eq!(
        calculate_next_try_time(finish_time, 7),
        finish_time + Duration::hours(128)
    );
    assert_eq!(
        calculate_next_try_time(finish_time, 8),
        finish_time + Duration::days(7)
    );
}
