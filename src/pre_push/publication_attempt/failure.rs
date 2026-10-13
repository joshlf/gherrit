//! Effect failure classification; never permission to retry or continue.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Outcome {
    BeforeSend,
    Rejected,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Operation {
    GitPublication,
    GraphqlMutation,
}

/// Downcastable context at a single effect boundary. Sources remain in the
/// report chain. Display preserves the existing CLI contract; structured
/// consumers need not classify prose or mistake an error for no effects.
#[derive(Debug)]
pub(super) struct EffectFailure {
    pub(super) outcome: Outcome,
    pub(super) operation: Operation,
    message: String,
}

impl EffectFailure {
    pub(super) fn new(outcome: Outcome, operation: Operation, message: impl Into<String>) -> Self {
        Self { outcome, operation, message: message.into() }
    }
}

impl fmt::Display for EffectFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // These fields are deliberately retained even in display-only clients.
        let _classification = (self.operation, self.outcome);
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for EffectFailure {}
