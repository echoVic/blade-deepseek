use std::fmt;

use orca_core::goal_runtime::{GoalUpdateIntent, GoalUsage, GoalVerificationResult};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalVerifierError {
    MissingEvidence,
    MissingBlocker,
    OversizedEvidence,
    Indeterminate(String),
}

impl fmt::Display for GoalVerifierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEvidence => formatter.write_str("terminal verification requires evidence"),
            Self::MissingBlocker => formatter.write_str("blocked verification requires a blocker"),
            Self::OversizedEvidence => {
                formatter.write_str("terminal verification evidence is too large")
            }
            Self::Indeterminate(message) => {
                write!(formatter, "terminal verification indeterminate: {message}")
            }
        }
    }
}

impl std::error::Error for GoalVerifierError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalVerifierOutput {
    pub result: GoalVerificationResult,
    pub usage: GoalUsage,
}

pub trait GoalVerifier: Send + Sync {
    fn verify(
        &self,
        objective: &str,
        intent: &GoalUpdateIntent,
    ) -> Result<GoalVerifierOutput, GoalVerifierError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicGoalVerifier;

impl GoalVerifier for DeterministicGoalVerifier {
    fn verify(
        &self,
        _objective: &str,
        intent: &GoalUpdateIntent,
    ) -> Result<GoalVerifierOutput, GoalVerifierError> {
        if intent.evidence.is_empty() {
            return Err(GoalVerifierError::MissingEvidence);
        }
        if intent.evidence.len() > 32
            || intent
                .evidence
                .iter()
                .any(|evidence| evidence.summary.chars().count() > 2_000)
        {
            return Err(GoalVerifierError::OversizedEvidence);
        }
        let result = match intent.requested_state {
            orca_core::goal_runtime::GoalRequestedState::Complete => {
                GoalVerificationResult::Achieved {
                    evidence: intent.evidence.clone(),
                }
            }
            orca_core::goal_runtime::GoalRequestedState::Blocked => {
                let blocker = intent
                    .blocker
                    .clone()
                    .ok_or(GoalVerifierError::MissingBlocker)?;
                GoalVerificationResult::Blocked { blocker }
            }
        };
        Ok(GoalVerifierOutput {
            result,
            usage: GoalUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::goal_runtime::{EvidenceItem, GoalRequestedState, IntentId};

    #[test]
    fn deterministic_verifier_requires_evidence_before_achieved() {
        let verifier = DeterministicGoalVerifier;
        let intent = GoalUpdateIntent {
            intent_id: IntentId::new(),
            requested_state: GoalRequestedState::Complete,
            reason: "done".to_string(),
            evidence: Vec::new(),
            blocker: None,
        };
        assert_eq!(
            verifier.verify("ship it", &intent).unwrap_err(),
            GoalVerifierError::MissingEvidence
        );
    }

    #[test]
    fn deterministic_verifier_preserves_typed_terminal_result() {
        let verifier = DeterministicGoalVerifier;
        let intent = GoalUpdateIntent {
            intent_id: IntentId::new(),
            requested_state: GoalRequestedState::Complete,
            reason: "done".to_string(),
            evidence: vec![EvidenceItem::observation("tests passed")],
            blocker: None,
        };
        let output = verifier.verify("ship it", &intent).unwrap();
        assert!(matches!(
            output.result,
            GoalVerificationResult::Achieved { .. }
        ));
    }
}
