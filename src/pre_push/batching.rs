use std::{num::NonZeroUsize, ops::Range};

use serde_json::Value;

pub(super) const INITIAL_GRAPHQL_BATCH_LEN: NonZeroUsize = NonZeroUsize::new(64).unwrap();
pub(super) const MAX_GRAPHQL_QUERY_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub(super) struct BatchPlan {
    item_count: usize,
    cursor: usize,
    batch_len: NonZeroUsize,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Backoff {
    pub attempted: usize,
    pub retry: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ItemTooLarge {
    pub index: usize,
}

impl BatchPlan {
    pub fn new(item_count: usize, max_batch_len: NonZeroUsize) -> Self {
        Self { item_count, cursor: 0, batch_len: max_batch_len }
    }

    pub fn current(&self) -> Option<Range<usize>> {
        (self.cursor < self.item_count)
            .then(|| self.cursor..self.item_count.min(self.cursor + self.batch_len.get()))
    }

    pub fn accept(&mut self) {
        self.cursor = self.current().expect("cannot accept a completed batch plan").end;
    }

    pub fn reject(&mut self) -> Result<Backoff, ItemTooLarge> {
        let range = self.current().expect("cannot reject a completed batch plan");
        let attempted = range.len();
        if attempted == 1 {
            return Err(ItemTooLarge { index: range.start });
        }

        let retry = NonZeroUsize::new(attempted / 2).expect("rejected batch has multiple items");
        self.batch_len = retry;
        Ok(Backoff { attempted, retry: retry.get() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseDisposition {
    Success,
    RetryLimit,
    Reobserve,
    Fatal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReplaySafety {
    /// The identical request is accepted and has the same effect after any
    /// successfully applied prefix.
    BlindRetrySafe,
    /// A partial result must be observed and replanned before another write.
    RequiresReobservation,
}

pub(super) fn query_exceeds_limit(query: &str) -> bool {
    query.len() > MAX_GRAPHQL_QUERY_BYTES
}

pub(super) fn classify_response(
    response: &Value,
    replay_safety: ReplaySafety,
) -> ResponseDisposition {
    let Some(errors) = response.get("errors") else {
        return ResponseDisposition::Success;
    };
    let has_only_resource_errors = errors
        .as_array()
        .is_some_and(|errors| !errors.is_empty() && errors.iter().all(is_resource_limit_error));

    match (has_only_resource_errors, replay_safety) {
        (true, ReplaySafety::BlindRetrySafe) => ResponseDisposition::RetryLimit,
        (true, ReplaySafety::RequiresReobservation) => ResponseDisposition::Reobserve,
        (false, _) => ResponseDisposition::Fatal,
    }
}

fn is_resource_limit_error(error: &Value) -> bool {
    let is_typed_resource_error = matches!(
        error.get("type").and_then(Value::as_str),
        Some("RESOURCE_LIMITS_EXCEEDED" | "MAX_NODE_LIMIT_EXCEEDED")
    );
    // HEURISTIC: GitHub middleware has also returned this parse error after
    // silently dropping or truncating an oversized request.
    let is_oversized_request_error = matches!(
        error.get("message").and_then(Value::as_str),
        Some("A query attribute must be specified and must be a string.")
    );

    is_typed_resource_error || is_oversized_request_error
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ranges(item_count: usize) -> Vec<Range<usize>> {
        let mut plan = BatchPlan::new(item_count, INITIAL_GRAPHQL_BATCH_LEN);
        let mut ranges = Vec::new();
        while let Some(range) = plan.current() {
            ranges.push(range);
            plan.accept();
        }
        ranges
    }

    #[test]
    fn plans_success_boundaries() {
        assert_eq!(ranges(0), Vec::<Range<usize>>::new());
        for (item_count, expected) in [(1, 0..1), (63, 0..63), (64, 0..64)] {
            assert_eq!(ranges(item_count), std::iter::once(expected).collect::<Vec<_>>());
        }
        assert_eq!(ranges(65), [0..64, 64..65]);
        assert_eq!(ranges(128), [0..64, 64..128]);
    }

    #[test]
    fn backs_off_without_advancing() {
        let mut plan = BatchPlan::new(64, INITIAL_GRAPHQL_BATCH_LEN);

        for (attempted, retry) in [(64, 32), (32, 16), (16, 8), (8, 4), (4, 2), (2, 1)] {
            assert_eq!(plan.current(), Some(0..attempted));
            assert_eq!(plan.reject(), Ok(Backoff { attempted, retry }));
            assert_eq!(plan.current(), Some(0..retry));
        }
        assert_eq!(plan.reject(), Err(ItemTooLarge { index: 0 }));
    }

    #[test]
    fn halves_the_actual_tail_attempt() {
        let mut plan = BatchPlan::new(100, INITIAL_GRAPHQL_BATCH_LEN);
        assert_eq!(plan.current(), Some(0..64));
        plan.accept();

        assert_eq!(plan.current(), Some(64..100));
        assert_eq!(plan.reject(), Ok(Backoff { attempted: 36, retry: 18 }));
        assert_eq!(plan.current(), Some(64..82));
    }

    #[test]
    fn rejects_a_single_tail_item_immediately() {
        let mut plan = BatchPlan::new(65, INITIAL_GRAPHQL_BATCH_LEN);
        plan.accept();

        assert_eq!(plan.current(), Some(64..65));
        assert_eq!(plan.reject(), Err(ItemTooLarge { index: 64 }));
    }

    #[test]
    fn preserves_a_reduced_batch_length_after_success() {
        let mut plan = BatchPlan::new(100, INITIAL_GRAPHQL_BATCH_LEN);
        plan.reject().unwrap();
        assert_eq!(plan.current(), Some(0..32));
        plan.accept();
        assert_eq!(plan.current(), Some(32..64));
    }

    #[test]
    fn floors_odd_backoff_lengths() {
        let mut plan = BatchPlan::new(3, NonZeroUsize::new(3).unwrap());
        assert_eq!(plan.reject(), Ok(Backoff { attempted: 3, retry: 1 }));
        assert_eq!(plan.current(), Some(0..1));
    }

    #[test]
    fn query_limit_is_inclusive() {
        assert!(!query_exceeds_limit(&"x".repeat(MAX_GRAPHQL_QUERY_BYTES)));
        assert!(query_exceeds_limit(&"x".repeat(MAX_GRAPHQL_QUERY_BYTES + 1)));
    }

    #[test]
    fn classifies_resource_limit_responses() {
        for error in [
            json!({ "type": "RESOURCE_LIMITS_EXCEEDED" }),
            json!({ "type": "MAX_NODE_LIMIT_EXCEEDED" }),
            json!({
                "message": "A query attribute must be specified and must be a string."
            }),
        ] {
            for response in
                [json!({ "errors": [error.clone()] }), json!({ "data": null, "errors": [error] })]
            {
                assert_eq!(
                    classify_response(&response, ReplaySafety::BlindRetrySafe),
                    ResponseDisposition::RetryLimit
                );
                assert_eq!(
                    classify_response(&response, ReplaySafety::RequiresReobservation),
                    ResponseDisposition::Reobserve
                );
            }
        }
    }

    #[test]
    fn retries_partial_resource_limit_responses() {
        let resource_error = json!({
            "path": ["op1"],
            "type": "RESOURCE_LIMITS_EXCEEDED"
        });

        let response = json!({
            "data": { "op0": { "value": 1 }, "op1": null },
            "errors": [resource_error]
        });

        assert_eq!(
            classify_response(&response, ReplaySafety::BlindRetrySafe),
            ResponseDisposition::RetryLimit
        );
        assert_eq!(
            classify_response(&response, ReplaySafety::RequiresReobservation),
            ResponseDisposition::Reobserve
        );
    }

    #[test]
    fn treats_mixed_or_malformed_errors_as_fatal() {
        let resource_error = json!({ "type": "RESOURCE_LIMITS_EXCEEDED" });
        let fatal_error = json!({ "type": "FORBIDDEN" });

        for response in [
            json!({ "errors": [fatal_error.clone()] }),
            json!({ "errors": [resource_error.clone(), fatal_error] }),
            json!({ "errors": [] }),
            json!({ "errors": "not an array" }),
        ] {
            for safety in [ReplaySafety::BlindRetrySafe, ReplaySafety::RequiresReobservation] {
                assert_eq!(classify_response(&response, safety), ResponseDisposition::Fatal);
            }
        }
    }

    #[test]
    fn accepts_responses_without_errors() {
        for safety in [ReplaySafety::BlindRetrySafe, ReplaySafety::RequiresReobservation] {
            assert_eq!(
                classify_response(&json!({ "data": {} }), safety),
                ResponseDisposition::Success
            );
        }
    }
}
