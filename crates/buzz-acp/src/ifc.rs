//! Client-side information-flow control for ACP agents.
//!
//! The idea: the client, not the agent, decides where data may go. ACP already
//! asks the client for permission before every tool call and tells it which tool
//! is being called, so the client can track what the agent has read and refuse a
//! later call that would move it somewhere it does not belong. An agent that has
//! never heard of IFC still gets gated, because the gate is the handshake.
//!
//! Two labels, which is enough to show the shape:
//!
//! * `Public`     — may go anywhere.
//! * `Confidential` — may not reach a public destination.
//!
//! Three decisions per tool call:
//!
//! * [`Decision::Allow`] — no flow worth tracking.
//! * [`Decision::Hide`]  — a confidential read. Allow the call, but ask the
//!   agent to keep the result out of the model's context behind an opaque ref.
//!   Only the ref carries the label, so the conversation stays usable.
//! * [`Decision::Block`] — a public write that would carry confidential data.
//!
//! The one thing the client cannot do alone is un-see a value for a model it
//! does not host, so [`Decision::Hide`] is a request the agent must honor. Flow
//! blocking needs no cooperation: the client simply answers "no".
//!
//! Enabled with `BUZZ_ACP_IFC=1`; otherwise every call is [`Decision::Allow`]
//! and this module does nothing.

use std::collections::HashMap;

/// What a value is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Public,
    Confidential,
}

impl Label {
    /// Combining anything with confidential data yields confidential data.
    fn join(self, other: Label) -> Label {
        if self == Label::Confidential || other == Label::Confidential {
            Label::Confidential
        } else {
            Label::Public
        }
    }
}

/// What the client decided about one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow { because: String },
    Hide { hide_ref: String, because: String },
    Block { because: String },
}

/// How a tool participates in information flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Reads data into the agent, at this label.
    Source(Label),
    /// Writes data out, to readers at this label.
    Sink(Label),
    /// Neither; nothing to track.
    Neutral,
}

/// The policy: which tools read secrets, which tools publish.
///
/// A real deployment would derive this from a catalog or from ACP's own
/// `kind` field. Hard-coding the demo server's two tools keeps the mechanism
/// visible instead of burying it in inference rules.
fn role_of(tool_name: &str) -> Role {
    // Tool names arrive prefixed by their MCP server, e.g. `ifcdemo__publish`.
    match tool_name.rsplit("__").next().unwrap_or(tool_name) {
        "read_secret" => Role::Source(Label::Confidential),
        "publish" => Role::Sink(Label::Public),
        _ => Role::Neutral,
    }
}

/// Per-session flow state: what the agent has read, and what each ref holds.
#[derive(Debug, Default)]
struct Session {
    /// The join of every label the model has actually seen.
    conversation: Option<Label>,
    /// Refs minted for withheld values, and the label each one carries.
    refs: HashMap<String, Label>,
    next_ref: usize,
}

impl Session {
    fn conversation(&self) -> Label {
        self.conversation.unwrap_or(Label::Public)
    }

    fn mint_ref(&mut self, label: Label) -> String {
        self.next_ref += 1;
        let name = format!("ifc-ref-{}", self.next_ref);
        self.refs.insert(name.clone(), label);
        name
    }

    /// The label of an outbound call: the conversation, plus any refs named in
    /// the arguments. An unrecognized `ifc-ref-*` is treated as confidential so
    /// a guessed or stale ref cannot launder data out.
    fn label_for_arguments(&self, arguments: &serde_json::Value) -> Label {
        let text = arguments.to_string();
        let mut label = self.conversation();
        for (name, ref_label) in &self.refs {
            if text.contains(name.as_str()) {
                label = label.join(*ref_label);
            }
        }
        if text.contains("ifc-ref-") {
            let unknown = !self.refs.keys().any(|name| text.contains(name.as_str()));
            if unknown {
                label = label.join(Label::Confidential);
            }
        }
        label
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

        let session = self.sessions.entry(session_id.to_string()).or_default();

        match role_of(tool_name) {
            Role::Neutral => Decision::Allow {
                because: "no information flow".to_string(),
            },

            Role::Source(Label::Public) => Decision::Allow {
                because: "public data".to_string(),
            },

            Role::Source(Label::Confidential) => {
                if self.agent_honors_hide {
                    // Keep it out of the model entirely. Only the ref is
                    // labeled, so the conversation stays public and useful.
                    let hide_ref = session.mint_ref(Label::Confidential);
                    Decision::Hide {
                        because: "confidential source withheld from the model".to_string(),
                        hide_ref,
                    }
                } else {
                    // The agent cannot withhold it, so the model will see it and
                    // the whole conversation becomes confidential.
                    session.conversation = Some(session.conversation().join(Label::Confidential));
                    Decision::Allow {
                        because: "confidential source; agent cannot hide, conversation tainted"
                            .to_string(),
                    }
                }
            }

            Role::Sink(Label::Public) => {
                if session.label_for_arguments(arguments) == Label::Confidential {
                    Decision::Block {
                        because: "would publish confidential data to a public destination"
                            .to_string(),
                    }
                } else {
                    Decision::Allow {
                        because: "public data to a public destination".to_string(),
                    }
                }
            }

            Role::Sink(Label::Confidential) => Decision::Allow {
                because: "destination is not public".to_string(),
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
