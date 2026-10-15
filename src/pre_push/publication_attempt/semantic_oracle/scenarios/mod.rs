use super::*;

// Outcome failures retain the effect batches which reached that outcome.
macro_rules! assert_attempt_outcome {
    ($report:expr, $expected:expr $(,)?) => {{
        let report = &$report;
        assert_eq!(report.outcome, $expected, "Attempt trace: {:#?}", report.trace);
    }};
    ($report:expr, $expected:expr, $($message:tt)+) => {{
        let report = &$report;
        assert_eq!(report.outcome, $expected, "{}\nAttempt trace: {:#?}",
            format_args!($($message)+), report.trace);
    }};
}

mod competition;
mod lifecycle;
mod namespace;
mod recovery;
