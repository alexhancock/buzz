//! Client-side information-flow control for ACP agents.
//!
//! The idea: the client, not the agent, decides where data may go. ACP already
//! asks the client for permission before every tool call and tells it which tool
//! is being called, so the client can track what the agent has read and refuse a
//! later call that would move it somewhere it does not belong. An agent that has
//! never heard of IFC still gets gated, because the gate is the handshake.
//!
//! The lattice itself is [`ifc_core`]: reader-set confidentiality labels, a
//! monotonic [`FlowState`] that accumulates everything the agent has seen, and
//! an egress check. This module supplies only the ACP-specific parts — which
//! tool is a source, which is a sink, and what to answer the agent.
//!
//! Three decisions per tool call:
//!
//! * [`Decision::Allow`] — the flow is permitted.
//! * [`Decision::Hide`]  — a confidential read. Allow the call, but ask the
//!   agent to keep the result out of the model's context behind an opaque ref.
//!   Only the ref carries the label, so the conversation stays usable.
//! * [`Decision::Block`] — a write whose destination would widen the readers.
//!
//! The one thing the client cannot do alone is un-see a value for a model it
//! does not host, so [`Decision::Hide`] is a request the agent must honor. Flow
//! blocking needs no cooperation: the client simply answers "no".
//!
//! Enabled with `BUZZ_ACP_IFC=1`; otherwise every call is [`Decision::Allow`]
//! and this module does nothing.

use ifc_core::{ConfidentialityLabel, EgressError, FlowState};
use std::collections::HashMap;

/// Every label in this demo lives in one universe.
const UNIVERSE: &str = "buzz-acp";

/// Who may read a value. `Principal` is a plain string for the demo; a real
/// deployment would use channel members or pubkeys.
type Label = ConfidentialityLabel<&'static str, String>;

/// The demo's one authorized reader of confidential data.
const OWNER: &str = "owner";

fn public() -> Label {
    ConfidentialityLabel::public(UNIVERSE)
}

fn confidential() -> Label {
    ConfidentialityLabel::restricted_to(UNIVERSE, OWNER.to_string())
}

/// What the client decided about one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow { because: String },
    Hide { hide_ref: String, because: String },
    Block { because: String },
}

/// How a tool participates in information flow.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Role {
    /// Reads data into the agent, at this label.
    Source(Label),
    /// Writes data out, readable by whoever this label allows.
    Sink(Label),
    /// Neither; nothing to track.
    Neutral,
}

/// The policy: which tools read secrets, which tools publish.
///
/// A real deployment would derive this from a catalog or from ACP's own `kind`
/// field. Hard-coding the demo server's two tools keeps the mechanism visible.
fn role_of(tool_name: &str) -> Role {
    // Tool names arrive prefixed by their MCP server, e.g. `ifcdemo__publish`.
    match tool_name.rsplit("__").next().unwrap_or(tool_name) {
        "read_secret" => Role::Source(confidential()),
        "publish" => Role::Sink(public()),
        _ => Role::Neutral,
    }
}

/// Per-session flow state.
///
/// The accumulated label lives in [`FlowState`], which is monotonic by
/// construction and deliberately not cloneable — a caller cannot stash a clean
/// copy and later use it to forget what the agent has seen.
#[derive(Debug, Default)]
struct Session {
    flow: FlowState<&'static str, String>,
    /// Refs minted for withheld values, and the label each one carries.
    refs: HashMap<String, Label>,
    next_ref: usize,
}

impl Session {
    fn mint_ref(&mut self, label: Label) -> String {
        self.next_ref += 1;
        let name = format!("ifc-ref-{}", self.next_ref);
        self.refs.insert(name.clone(), label);
        name
    }

    /// Check an outbound call against everything the agent has seen, plus any
    /// refs named in the arguments.
    ///
    /// Refs are checked against a scratch [`FlowState`] seeded from the
    /// session's own: a ref mentioned in one call must not taint the session
    /// permanently, only the call that used it.
    fn check_egress(
        &self,
        destination: &Label,
        arguments: &serde_json::Value,
    ) -> Result<(), EgressError> {
        let text = arguments.to_string();

        let mut scratch = FlowState::default();
        if let Some(seen) = self.flow.accumulated_label() {
            scratch.observe(seen);
        }
        if self.flow.has_unresolved_input() {
            scratch.mark_unknown();
        }

        let mut named_known = false;
        for (name, label) in &self.refs {
            if text.contains(name.as_str()) {
                scratch.observe(label);
                named_known = true;
            }
        }
        // A guessed, stale, or cross-session ref has no label here. Treat it as
        // unresolved rather than as absent, so it fails closed.
        if text.contains("ifc-ref-") && !named_known {
            scratch.mark_unknown();
        }

        scratch.check_egress(destination)
    }
}

/// The client-side lattice. One per agent connection.
#[derive(Debug, Default)]
pub struct Ifc {
    enabled: bool,
    /// Whether the agent said it honors the hide directive.
    agent_honors_hide: bool,
    sessions: HashMap<String, Session>,
}

impl Ifc {
    /// Read the on/off switch from the environment.
    pub fn from_env() -> Self {
        let enabled = matches!(
            std::env::var("BUZZ_ACP_IFC").unwrap_or_default().as_str(),
            "1" | "true" | "TRUE" | "yes"
        );
        Self {
            enabled,
            ..Default::default()
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record whether the agent advertised `ifc.hideDirective` at initialize.
    pub fn set_agent_honors_hide(&mut self, honors: bool) {
        self.agent_honors_hide = honors;
    }

    /// Decide what to do about one tool call.
    pub fn decide(
        &mut self,
        session_id: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Decision {
        if !self.enabled {
            return Decision::Allow {
                because: "ifc disabled".to_string(),
            };
        }

        let honors_hide = self.agent_honors_hide;
        let session = self.sessions.entry(session_id.to_string()).or_default();

        match role_of(tool_name) {
            Role::Neutral => Decision::Allow {
                because: "no information flow".to_string(),
            },

            Role::Source(label) if label.is_public() => Decision::Allow {
                because: "public data".to_string(),
            },

            Role::Source(label) => {
                if honors_hide {
                    // Keep it out of the model entirely. Only the ref is
                    // labeled, so the session stays usable for public work.
                    let hide_ref = session.mint_ref(label);
                    Decision::Hide {
                        because: "confidential source withheld from the model".to_string(),
                        hide_ref,
                    }
                } else {
                    // The agent cannot withhold it, so the model will see it and
                    // the whole session inherits the label. `FlowState` is
                    // monotonic: there is no way to undo this.
                    session.flow.observe(&label);
                    Decision::Allow {
                        because: "confidential source; agent cannot hide, session tainted"
                            .to_string(),
                    }
                }
            }

            Role::Sink(destination) => match session.check_egress(&destination, arguments) {
                Ok(()) => Decision::Allow {
                    because: "flow permitted to this destination".to_string(),
                },
                Err(EgressError::DestinationWidensReaders) => Decision::Block {
                    because: "would widen the readers of confidential data".to_string(),
                },
                Err(EgressError::UnresolvedInput) => Decision::Block {
                    because: "input of unknown label cannot be released".to_string(),
                },
                Err(other) => Decision::Block {
                    because: format!("egress refused: {other}"),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> serde_json::Value {
        json
    }

    /// An agent that honors hiding: the secret never enters the model, so the
    /// conversation stays public and ordinary work still succeeds.
    #[test]
    fn hides_a_confidential_read_then_blocks_the_ref_from_a_public_sink() {
        let mut ifc = Ifc {
            enabled: true,
            agent_honors_hide: true,
            ..Default::default()
        };

        let decision = ifc.decide("s1", "ifcdemo__read_secret", &args(serde_json::json!({})));
        let hide_ref = match decision {
            Decision::Hide { hide_ref, .. } => hide_ref,
            other => panic!("expected Hide, got {other:?}"),
        };

        // Handing the ref to a public sink is refused.
        let blocked = ifc.decide(
            "s1",
            "ifcdemo__publish",
            &args(serde_json::json!({ "message": hide_ref })),
        );
        assert!(matches!(blocked, Decision::Block { .. }), "{blocked:?}");

        // Unrelated public work is unaffected.
        let allowed = ifc.decide(
            "s1",
            "ifcdemo__publish",
            &args(serde_json::json!({ "message": "the weather is fine" })),
        );
        assert!(matches!(allowed, Decision::Allow { .. }), "{allowed:?}");
    }

    /// An agent that cannot hide still gets flow-gated: the model sees the
    /// secret, so everything afterwards is confidential.
    #[test]
    fn taints_the_conversation_when_the_agent_cannot_hide() {
        let mut ifc = Ifc {
            enabled: true,
            agent_honors_hide: false,
            ..Default::default()
        };

        let read = ifc.decide("s1", "ifcdemo__read_secret", &args(serde_json::json!({})));
        assert!(matches!(read, Decision::Allow { .. }), "{read:?}");

        // Now even innocuous-looking text is refused: the model knows the secret.
        let blocked = ifc.decide(
            "s1",
            "ifcdemo__publish",
            &args(serde_json::json!({ "message": "nothing to see" })),
        );
        assert!(matches!(blocked, Decision::Block { .. }), "{blocked:?}");
    }

    #[test]
    fn a_guessed_or_stale_ref_fails_closed() {
        let mut ifc = Ifc {
            enabled: true,
            agent_honors_hide: true,
            ..Default::default()
        };
        // Never minted in this session.
        let blocked = ifc.decide(
            "s1",
            "ifcdemo__publish",
            &args(serde_json::json!({ "message": "ifc-ref-99" })),
        );
        assert!(matches!(blocked, Decision::Block { .. }), "{blocked:?}");
    }

    #[test]
    fn sessions_do_not_share_refs() {
        let mut ifc = Ifc {
            enabled: true,
            agent_honors_hide: true,
            ..Default::default()
        };
        let hide_ref = match ifc.decide("s1", "ifcdemo__read_secret", &args(serde_json::json!({})))
        {
            Decision::Hide { hide_ref, .. } => hide_ref,
            other => panic!("expected Hide, got {other:?}"),
        };
        // The same string in a different session is an unknown ref, so it is
        // still refused — fail closed, not open.
        let blocked = ifc.decide(
            "s2",
            "ifcdemo__publish",
            &args(serde_json::json!({ "message": hide_ref })),
        );
        assert!(matches!(blocked, Decision::Block { .. }), "{blocked:?}");
    }

    /// The payoff of reader sets over a two-value label: narrowing is allowed.
    ///
    /// A two-label model can only say "confidential, so nowhere public". Reader
    /// sets can say "readable by alice and bob, so alice alone is fine, but
    /// adding carol is not" — which is the distinction real audiences need.
    #[test]
    fn narrowing_readers_is_allowed_but_widening_is_not() {
        use std::collections::BTreeSet;

        let alice_and_bob = ConfidentialityLabel::restricted(
            UNIVERSE,
            BTreeSet::from(["alice".to_string(), "bob".to_string()]),
        )
        .expect("non-empty");
        let alice_only = ConfidentialityLabel::restricted_to(UNIVERSE, "alice".to_string());
        let plus_carol = ConfidentialityLabel::restricted(
            UNIVERSE,
            BTreeSet::from(["alice".to_string(), "bob".to_string(), "carol".to_string()]),
        )
        .expect("non-empty");

        let mut session = Session::default();
        session.flow.observe(&alice_and_bob);
        let nothing = serde_json::json!({});

        // Narrowing to a subset of the authorized readers is safe.
        assert!(session.check_egress(&alice_only, &nothing).is_ok());
        // Adding a reader is not, and neither is going fully public.
        assert!(session.check_egress(&plus_carol, &nothing).is_err());
        assert!(session.check_egress(&public(), &nothing).is_err());
    }

    #[test]
    fn disabled_by_default_allows_everything() {
        let mut ifc = Ifc::default();
        assert!(!ifc.is_enabled());
        for tool in ["ifcdemo__read_secret", "ifcdemo__publish"] {
            let d = ifc.decide("s1", tool, &args(serde_json::json!({ "message": "x" })));
            assert!(matches!(d, Decision::Allow { .. }), "{tool}: {d:?}");
        }
    }
}
