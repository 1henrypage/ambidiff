//! Comment lifecycle state machine: the verify loop.
//!
//! open -> addressed (agent, optional one-line response)
//! addressed -> resolved (HUMAN ONLY) or reopened (human)
//! reopened -> addressed (agent), and so on.
//!
//! The agent's to-do list is exactly {open, reopened}. Resolve and reopen are
//! human-only judgments; address is open to anyone (typically the agent).
//! Transitions are id-keyed by callers, so file renames never gate lifecycle.

use serde::{Deserialize, Serialize};

/// Comment status as persisted in the review file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Open,
    Addressed,
    Resolved,
    Reopened,
}

impl Status {
    /// True when the comment is actionable by the agent.
    pub fn is_todo(self) -> bool {
        matches!(self, Status::Open | Status::Reopened)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Addressed => "addressed",
            Status::Resolved => "resolved",
            Status::Reopened => "reopened",
        }
    }
}

/// A lifecycle action requested on a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Address,
    Resolve,
    Reopen,
}

/// Who is performing the action. The CLI cannot verify humanity; human-only
/// enforcement is a contract: frontends pass Human for interactive users, the
/// agent instructions forbid agents from using resolve/reopen verbs, and
/// validation flags impossible histories where it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    Human,
    Agent,
}

/// Typed rejection of an illegal transition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    #[error("comment is already {status}")]
    AlreadyInState { status: &'static str },
    #[error("cannot {action} a {status} comment")]
    InvalidTransition {
        action: &'static str,
        status: &'static str,
    },
    #[error("only a human may {action} a comment")]
    HumanOnly { action: &'static str },
}

/// Apply `action` by `actor` to a comment in `status`, returning the next
/// status or a typed error. Pure: callers persist the result.
pub fn transition(status: Status, action: Action, actor: Actor) -> Result<Status, LifecycleError> {
    match action {
        Action::Address => match status {
            Status::Open | Status::Reopened => Ok(Status::Addressed),
            Status::Addressed => Err(LifecycleError::AlreadyInState {
                status: "addressed",
            }),
            Status::Resolved => Err(LifecycleError::InvalidTransition {
                action: "address",
                status: "resolved",
            }),
        },
        Action::Resolve => {
            if actor != Actor::Human {
                return Err(LifecycleError::HumanOnly { action: "resolve" });
            }
            match status {
                Status::Open | Status::Addressed | Status::Reopened => Ok(Status::Resolved),
                Status::Resolved => Err(LifecycleError::AlreadyInState { status: "resolved" }),
            }
        }
        Action::Reopen => {
            if actor != Actor::Human {
                return Err(LifecycleError::HumanOnly { action: "reopen" });
            }
            match status {
                Status::Addressed | Status::Resolved => Ok(Status::Reopened),
                Status::Open => Err(LifecycleError::AlreadyInState { status: "open" }),
                Status::Reopened => Err(LifecycleError::AlreadyInState { status: "reopened" }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;
    use Actor::*;
    use Status::*;

    /// Exhaustive transition table: 4 states x 3 actions x 2 actors.
    #[test]
    fn exhaustive_transition_table() {
        let cases: &[(Status, Action, Actor, Result<Status, ()>)] = &[
            // Address: anyone, from open/reopened only.
            (Open, Address, Agent, Ok(Addressed)),
            (Open, Address, Human, Ok(Addressed)),
            (Reopened, Address, Agent, Ok(Addressed)),
            (Reopened, Address, Human, Ok(Addressed)),
            (Addressed, Address, Agent, Err(())),
            (Addressed, Address, Human, Err(())),
            (Resolved, Address, Agent, Err(())),
            (Resolved, Address, Human, Err(())),
            // Resolve: human only, from any non-resolved state.
            (Open, Resolve, Human, Ok(Resolved)),
            (Addressed, Resolve, Human, Ok(Resolved)),
            (Reopened, Resolve, Human, Ok(Resolved)),
            (Resolved, Resolve, Human, Err(())),
            (Open, Resolve, Agent, Err(())),
            (Addressed, Resolve, Agent, Err(())),
            (Reopened, Resolve, Agent, Err(())),
            (Resolved, Resolve, Agent, Err(())),
            // Reopen: human only, from addressed/resolved.
            (Addressed, Reopen, Human, Ok(Reopened)),
            (Resolved, Reopen, Human, Ok(Reopened)),
            (Open, Reopen, Human, Err(())),
            (Reopened, Reopen, Human, Err(())),
            (Addressed, Reopen, Agent, Err(())),
            (Resolved, Reopen, Agent, Err(())),
            (Open, Reopen, Agent, Err(())),
            (Reopened, Reopen, Agent, Err(())),
        ];
        assert_eq!(cases.len(), 24, "table covers 4x3x2");
        for (status, action, actor, expected) in cases {
            let got = transition(*status, *action, *actor);
            match expected {
                Ok(next) => assert_eq!(
                    got.as_ref().ok(),
                    Some(next),
                    "{status:?} + {action:?} by {actor:?}"
                ),
                Err(()) => assert!(got.is_err(), "{status:?} + {action:?} by {actor:?}"),
            }
        }
    }

    #[test]
    fn agent_resolve_is_rejected_with_human_only_error() {
        assert_eq!(
            transition(Addressed, Resolve, Agent),
            Err(LifecycleError::HumanOnly { action: "resolve" })
        );
    }

    #[test]
    fn as_str_round_trips_every_status() {
        for (status, name) in [
            (Open, "open"),
            (Addressed, "addressed"),
            (Resolved, "resolved"),
            (Reopened, "reopened"),
        ] {
            assert_eq!(status.as_str(), name);
            // as_str must agree with the serde wire form.
            assert_eq!(
                serde_json::to_value(status).expect("json"),
                serde_json::Value::String(name.to_string())
            );
        }
    }

    #[test]
    fn todo_covers_open_and_reopened() {
        assert!(Open.is_todo());
        assert!(Reopened.is_todo());
        assert!(!Addressed.is_todo());
        assert!(!Resolved.is_todo());
    }
}
