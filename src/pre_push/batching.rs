//! Choose request sizes without encoding GraphQL or using the network.
//!
//! BatchPlan tracks a position in the original item order. Accepting a batch
//! advances it. Rejecting a batch shrinks the range without advancing. Keep the
//! smaller size as the cap for the rest of this invocation.

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

/// One nonempty range borrowed from a plan. Accepting or rejecting it consumes
/// the attempt, so it cannot be resolved twice or against a different range.
/// Dropping it leaves the plan unchanged, as when decoding a response fails.
#[must_use]
pub(super) struct BatchAttempt<'plan> {
    plan: &'plan mut BatchPlan,
    range: Range<usize>,
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

    pub fn current(&mut self) -> Option<BatchAttempt<'_>> {
        if self.cursor == self.item_count {
            return None;
        }
        // Limit the increment before adding it. cursor + batch_len may overflow
        // even though the smaller final range fits within item_count.
        let start = self.cursor;
        let length = self.batch_len.get().min(self.item_count - start);
        Some(BatchAttempt { plan: self, range: start..start + length })
    }
}

impl BatchAttempt<'_> {
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    pub fn accept(self) {
        self.plan.cursor = self.range.end;
    }

    pub fn reject(self) -> Result<Backoff, ItemTooLarge> {
        // Retrying one item cannot make its document smaller. Report its
        // original position instead of retrying forever at the smallest size.
        let attempted = self.range.len();
        if attempted == 1 {
            return Err(ItemTooLarge { index: self.range.start });
        }

        let retry = NonZeroUsize::new(attempted / 2).expect("rejected batch has multiple items");
        self.plan.batch_len = retry;
        Ok(Backoff { attempted, retry: retry.get() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseDisposition {
    Success,
    RetryLimit,
    Fatal,
}

pub(super) fn query_exceeds_limit(query: &str) -> bool {
    query.len() > MAX_GRAPHQL_QUERY_BYTES
}

pub(super) fn classify_response(response: &Value) -> ResponseDisposition {
    // Partial data or mixed errors may follow mutations that already took
    // effect. Do not retry them in smaller batches. Retry only the narrow error
    // shape checked here, with no data. This heuristic does not prove that a
    // failed mutation made no remote changes.
    let Some(errors) = response.get("errors") else {
        return ResponseDisposition::Success;
    };
    let has_no_data = response.get("data").is_none_or(Value::is_null);
    let has_only_resource_errors = errors
        .as_array()
        .is_some_and(|errors| !errors.is_empty() && errors.iter().all(is_resource_limit_error));

    if has_no_data && has_only_resource_errors {
        ResponseDisposition::RetryLimit
    } else {
        ResponseDisposition::Fatal
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

    fn current_range(plan: &mut BatchPlan) -> Option<Range<usize>> {
        plan.current().map(|attempt| attempt.range())
    }

    fn ranges(item_count: usize) -> Vec<Range<usize>> {
        let mut plan = BatchPlan::new(item_count, INITIAL_GRAPHQL_BATCH_LEN);
        let mut ranges = Vec::new();
        while let Some(attempt) = plan.current() {
            ranges.push(attempt.range());
            attempt.accept();
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
            assert_eq!(current_range(&mut plan), Some(0..attempted));
            assert_eq!(plan.current().unwrap().reject(), Ok(Backoff { attempted, retry }));
            assert_eq!(current_range(&mut plan), Some(0..retry));
        }
        assert_eq!(plan.current().unwrap().reject(), Err(ItemTooLarge { index: 0 }));
    }

    #[test]
    fn halves_the_actual_tail_attempt() {
        let mut plan = BatchPlan::new(100, INITIAL_GRAPHQL_BATCH_LEN);
        assert_eq!(current_range(&mut plan), Some(0..64));
        plan.current().unwrap().accept();

        assert_eq!(current_range(&mut plan), Some(64..100));
        assert_eq!(plan.current().unwrap().reject(), Ok(Backoff { attempted: 36, retry: 18 }));
        assert_eq!(current_range(&mut plan), Some(64..82));
    }

    #[test]
    fn rejects_a_single_tail_item_immediately() {
        let mut plan = BatchPlan::new(65, INITIAL_GRAPHQL_BATCH_LEN);
        plan.current().unwrap().accept();

        assert_eq!(current_range(&mut plan), Some(64..65));
        assert_eq!(plan.current().unwrap().reject(), Err(ItemTooLarge { index: 64 }));
    }

    #[test]
    fn preserves_a_reduced_batch_length_after_success() {
        let mut plan = BatchPlan::new(100, INITIAL_GRAPHQL_BATCH_LEN);
        plan.current().unwrap().reject().unwrap();
        assert_eq!(current_range(&mut plan), Some(0..32));
        plan.current().unwrap().accept();
        assert_eq!(current_range(&mut plan), Some(32..64));
    }

    #[test]
    fn floors_odd_backoff_lengths() {
        let mut plan = BatchPlan::new(3, NonZeroUsize::new(3).unwrap());
        assert_eq!(plan.current().unwrap().reject(), Ok(Backoff { attempted: 3, retry: 1 }));
        assert_eq!(current_range(&mut plan), Some(0..1));
    }

    #[test]
    fn dropping_an_attempt_does_not_skip_items() {
        let mut plan = BatchPlan::new(65, INITIAL_GRAPHQL_BATCH_LEN);
        {
            let attempt = plan.current().unwrap();
            assert_eq!(attempt.range(), 0..64);
        }
        assert_eq!(current_range(&mut plan), Some(0..64));
        plan.current().unwrap().accept();
        assert_eq!(current_range(&mut plan), Some(64..65));
    }

    #[test]
    fn extreme_counts_bound_the_increment_before_adding() {
        let mut plan = BatchPlan::new(usize::MAX, NonZeroUsize::new(usize::MAX - 1).unwrap());
        assert_eq!(current_range(&mut plan), Some(0..usize::MAX - 1));
        plan.current().unwrap().accept();
        assert_eq!(current_range(&mut plan), Some(usize::MAX - 1..usize::MAX));
        assert_eq!(plan.current().unwrap().reject(), Err(ItemTooLarge { index: usize::MAX - 1 }));
        plan.current().unwrap().accept();
        assert!(plan.current().is_none());
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
            assert_eq!(
                classify_response(&json!({ "errors": [error] })),
                ResponseDisposition::RetryLimit
            );
            assert_eq!(
                classify_response(&json!({ "data": null, "errors": [error] })),
                ResponseDisposition::RetryLimit
            );
        }
    }

    #[test]
    fn treats_partial_or_mixed_errors_as_fatal() {
        let resource_error = json!({ "type": "RESOURCE_LIMITS_EXCEEDED" });
        let fatal_error = json!({ "type": "FORBIDDEN" });

        for response in [
            json!({ "errors": [fatal_error.clone()] }),
            json!({ "errors": [resource_error.clone(), fatal_error] }),
            json!({ "errors": [] }),
            json!({ "errors": "not an array" }),
            json!({ "data": {}, "errors": [resource_error] }),
        ] {
            assert_eq!(classify_response(&response), ResponseDisposition::Fatal);
        }
    }

    #[test]
    fn accepts_responses_without_errors() {
        assert_eq!(classify_response(&json!({ "data": {} })), ResponseDisposition::Success);
    }
}
