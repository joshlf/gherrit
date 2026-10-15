//! Effect failure classification; never permission to retry or continue.

use std::fmt;

use crate::diagnostic::detail as safe;

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
    stage: Option<String>,
    targets: Vec<String>,
}

impl EffectFailure {
    pub(super) fn new(outcome: Outcome, operation: Operation, message: impl Into<String>) -> Self {
        Self { outcome, operation, message: message.into(), stage: None, targets: Vec::new() }
    }
}

impl fmt::Display for EffectFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // These fields are deliberately retained even in display-only clients.
        let _classification = (self.operation, self.outcome);
        formatter.write_str(&self.message)?;
        if let Some(stage) = &self.stage {
            write!(formatter, "\nPublication stage: {}", safe(stage))?;
            for target in self.targets.iter().take(20) {
                write!(formatter, "\n  {}", safe(target))?;
            }
            if self.targets.len() > 20 {
                write!(formatter, "\n  ({} more targets omitted)", self.targets.len() - 20)?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for EffectFailure {}

/// Preserve the typed failure and source chain, adding only safe identity
/// context. This modifies presentation, never acknowledgement or authority.
pub(super) fn contextualize(
    mut error: eyre::Report,
    stage: &str,
    targets: &[String],
) -> eyre::Report {
    if let Some(failure) = error.downcast_mut::<EffectFailure>() {
        failure.stage = Some(stage.to_owned());
        failure.targets = targets.to_vec();
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_is_terminal_safe_bounded_and_retains_typed_outcome() {
        let error: eyre::Report =
            EffectFailure::new(Outcome::Indeterminate, Operation::GitPublication, "indeterminate")
                .into();
        let targets = (0..25)
            .map(|_| "change Gone; refs/heads/name\u{1b}[31m\u{85}".to_owned())
            .collect::<Vec<_>>();
        let error = contextualize(error, "initial Git", &targets);
        let display = error.to_string();
        assert!(!display.contains('\u{1b}'));
        assert!(!display.contains('\u{85}'));
        assert!(display.contains("change Gone"));
        assert!(display.contains("5 more targets omitted"));
        assert_eq!(error.downcast_ref::<EffectFailure>().unwrap().outcome, Outcome::Indeterminate);
    }
}
