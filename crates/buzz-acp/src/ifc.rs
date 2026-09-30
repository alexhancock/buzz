//! Client-side information-flow control for ACP agents.
//!
//! The idea: the client, not the agent, decides where data may go. ACP already
//! asks the client for permission before every tool call and tells it which tool
//! is being called, so the client can track what the agent has read and refuse a
//! later call that would move it somewhere it does not belong. An agent that has
//! never heard of IFC still gets gated, because the gate is the handshake.
//!
//! The lattice itself is [`ifc_core`]. Two independent axes:
//!
//! * **Confidentiality** — reader sets. Who may learn a value. Accumulated in
//!   [`FlowState`] and checked against a *destination*: may this payload go
//!   there?
//! * **Integrity** — trusted or untrusted. Whether an adversary could have
//!   authored a value. Accumulated in [`IntegrityState`] and checked against
//!   *nothing*: it is a property of the context alone.
//!
//! From those come the paper's two policies, both enforced here:
//!
//! * **P-F (permitted flow)** guards the payload. Applied to every sink.
//! * **P-T (trusted action)** guards the control flow. Applied to every
//!   consequential tool, and deliberately blind to the call's arguments. P-T is
//!   a claim about *who decided to act*, not about what is being sent.
//!
//! P-T is what catches prompt injection. A confidentiality-only gate cannot:
//! when an injected email says "schedule a payment to the attacker", nothing
//! confidential leaks, no reader set widens, and P-F has no basis to object. The
//! harm is that the adversary chose the action. That is only visible on the
//! integrity axis.
//!
//! Three decisions per tool call:
//!
//! * [`Decision::Allow`] — the flow is permitted.
//! * [`Decision::Hide`] — a source worth keeping out of the context. Allow the
//!   call, but ask the agent to keep the result out of the model's context
//!   behind an opaque ref. Only the ref carries the label, so the context keeps
//!   both its confidentiality headroom *and* its integrity.
//! * [`Decision::Block`] — a write that would widen the readers (P-F), or a
//!   consequential action chosen in a tainted context (P-T).
//!
//! The one thing the client cannot do alone is un-see a value for a model it
//! does not host, so [`Decision::Hide`] is a request the agent must honor.
//! Neither policy needs the agent's cooperation to *block*: the client simply
//! answers "no".
//!
//! Enabled with `BUZZ_ACP_IFC=1`; otherwise every call is [`Decision::Allow`]
//! and this module does nothing.

use ifc_core::{ConfidentialityLabel, EgressError, FlowState, Integrity, IntegrityState};
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

/// Data a tool reads into the agent, on both axes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    confidentiality: Label,
    integrity: Integrity,
}

impl Source {
    /// Whether this source is worth keeping out of the model's context.
    ///
    /// Public *and* trusted data costs nothing to read: it constrains no future
    /// egress and forbids no future action. Anything else is worth hiding, for
    /// one of two quite different reasons.
    fn worth_hiding(&self) -> bool {
        !self.confidentiality.is_public() || !self.integrity.is_trusted()
    }

    fn describe(&self) -> &'static str {
        match (
            self.confidentiality.is_public(),
            self.integrity.is_trusted(),
        ) {
            (false, false) => "confidential untrusted",
            (false, true) => "confidential",
            (true, false) => "untrusted",
            (true, true) => "public trusted",
        }
    }
}

/// How a tool participates in information flow.
///
/// The three properties are independent, which is why this is a struct and not
/// an enum. `publish` is a sink *and* consequential. `schedule_payment` is
/// consequential but writes nothing readable outward, so P-F has nothing to say
/// about it and only P-T can stop it. `read_inbox` is a source and harmless to
/// call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ToolPolicy {
    /// Data the call reads into the agent.
    source: Option<Source>,
    /// Where the call writes, when it writes somewhere with an audience.
    sink: Option<Label>,
    /// Whether making the call changes the world. Subject to P-T.
    consequential: bool,
}

/// The policy: which tools read what, which publish, and which act.
///
/// A real deployment would derive this from a catalog or from ACP's own `kind`
/// field. Hard-coding the demo server's tools keeps the mechanism visible.
///
/// Note the remaining fail-open: an unrecognized tool is neutral and *not*
/// consequential, so it escapes P-T entirely. The paper's posture is the
/// opposite — label conservatively and apply P-T to every consequential tool —
/// and a real deployment should default `consequential` to true. The demo keeps
/// it permissive so that an agent's ordinary tools (shell, editor, search) stay
/// usable without a full catalog.
fn policy_for(tool_name: &str) -> ToolPolicy {
    // Tool names arrive prefixed by their MCP server, e.g. `ifcdemo__publish`.
    match tool_name.rsplit("__").next().unwrap_or(tool_name) {
        // The user's own secret: nobody else may read it, but the user wrote it,
        // so it is perfectly good grounds for a decision.
        "read_secret" => ToolPolicy {
            source: Some(Source {
                confidentiality: confidential(),
                integrity: Integrity::Trusted,
            }),
            ..Default::default()
        },

        // A stranger's message. The opposite diagonal: there is no secrecy
        // obligation at all — it is already public — but an adversary wrote it,
        // so nothing it says may be allowed to decide anything.
        "read_inbox" => ToolPolicy {
            source: Some(Source {
                confidentiality: public(),
                integrity: Integrity::Untrusted,
            }),
            ..Default::default()
        },

        // Writes where anyone can read. Both a sink and consequential.
        "publish" => ToolPolicy {
            sink: Some(public()),
            consequential: true,
            ..Default::default()
        },

        // Moves money. Consequential, but not an egress: its arguments are an
        // amount and a payee, and no reader set widens. This is the tool that
        // shows why confidentiality alone is not enough.
        "schedule_payment" => ToolPolicy {
            consequential: true,
            ..Default::default()
        },

        _ => ToolPolicy::default(),
    }
}

/// Per-session flow state.
///
/// Both accumulators are monotonic by construction and deliberately not
/// cloneable — a caller cannot stash a clean copy and later use it to forget
/// what the agent has seen.
#[derive(Debug, Default)]
struct Session {
    /// Confidentiality of everything the model has read.
    flow: FlowState<&'static str, String>,
    /// Trustworthiness of everything that has influenced the model.
    context: IntegrityState,
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

    /// Admit a source's result, hiding it when hiding buys anything.
    fn admit(&mut self, source: Source, honors_hide: bool) -> Decision {
        if !source.worth_hiding() {
            return Decision::Allow {
                because: "public, trusted data".to_string(),
            };
        }

        if honors_hide {
            // Hiding is what preserves *both* axes. The value never reaches the
            // model, so it neither constrains later egress nor taints the
            // context that chooses later actions. Note what is deliberately
            // absent here: no `context.observe`. That omission is the entire
            // reason a hidden untrusted read leaves consequential tools usable.
            let because = format!("{} source withheld from the model", source.describe());
            let hide_ref = self.mint_ref(source.confidentiality);
            return Decision::Hide { because, hide_ref };
        }

        // The agent cannot withhold it, so the model will see it and the context
        // inherits both labels. Both accumulators are monotonic: there is no
        // way to undo this.
        self.flow.observe(&source.confidentiality);
        self.context.observe(source.integrity);
        Decision::Allow {
            because: format!(
                "{} source; agent cannot hide, context tainted",
                source.describe()
            ),
        }
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
        let policy = policy_for(tool_name);

        // P-T (trusted action). Asked first, because it is a question about the
        // decision itself rather than about the data being moved, and it takes
        // no account of the arguments at all.
        if policy.consequential && session.context.check_trusted_action().is_err() {
            return Decision::Block {
                because: "consequential action chosen in a context tainted by untrusted data"
                    .to_string(),
            };
        }

        // P-F (permitted flow). Guards the payload.
        //
        // Note that P-T above did not look at `arguments`, and this does. An
        // untrusted value may still be *forwarded* — the user asked for that —
        // so long as its audience does not widen.
        if let Some(destination) = &policy.sink {
            if let Err(error) = session.check_egress(destination, arguments) {
                return Decision::Block {
                    because: match error {
                        EgressError::DestinationWidensReaders => {
                            "would widen the readers of confidential data".to_string()
                        }
                        EgressError::UnresolvedInput => {
                            "input of unknown label cannot be released".to_string()
                        }
                        other => format!("egress refused: {other}"),
                    },
                };
            }
        }

        if let Some(source) = policy.source {
            return session.admit(source, honors_hide);
        }

        Decision::Allow {
            because: if policy.sink.is_some() {
                "flow permitted to this destination".to_string()
            } else if policy.consequential {
                "consequential, but the context is trusted".to_string()
            } else {
                "no information flow".to_string()
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

    fn ifc(honors_hide: bool) -> Ifc {
        Ifc {
            enabled: true,
            agent_honors_hide: honors_hide,
            ..Default::default()
        }
    }

    // ---------------------------------------------------------------- P-F ----

    /// An agent that honors hiding: the secret never enters the model, so the
    /// conversation stays public and ordinary work still succeeds.
    #[test]
    fn hides_a_confidential_read_then_blocks_the_ref_from_a_public_sink() {
        let mut ifc = ifc(true);

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
        let mut ifc = ifc(false);

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
        let mut ifc = ifc(true);
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
        let mut ifc = ifc(true);
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

    // ---------------------------------------------------------------- P-T ----

    /// The attack a confidentiality-only gate cannot see, and the reason the
    /// integrity axis exists.
    ///
    /// An injected instruction arrives in a public inbox and tells the agent to
    /// move money. Nothing confidential is involved anywhere in this trace, so
    /// P-F has no basis to object: `schedule_payment` is not a sink, and even if
    /// it were, no reader set widens. Only P-T stops it, because only P-T asks
    /// who chose the action.
    #[test]
    fn blocks_a_consequential_action_chosen_from_untrusted_input() {
        let mut ifc = ifc(false);

        let read = ifc.decide("s1", "ifcdemo__read_inbox", &args(serde_json::json!({})));
        assert!(matches!(read, Decision::Allow { .. }), "{read:?}");

        let blocked = ifc.decide(
            "s1",
            "ifcdemo__schedule_payment",
            &args(serde_json::json!({ "amount": 5000, "payee": "attacker" })),
        );
        assert!(
            matches!(blocked, Decision::Block { .. }),
            "P-T must refuse an action chosen from untrusted input: {blocked:?}"
        );

        // Confirm the claim above: the same call is *not* an egress violation.
        // P-F genuinely has nothing to say here, so P-T is doing all the work.
        let session = &ifc.sessions["s1"];
        assert!(
            session
                .check_egress(&public(), &serde_json::json!({}))
                .is_ok(),
            "reading the public inbox must not restrict egress at all"
        );
    }

    /// Before any untrusted read, the same consequential call is fine. P-T is
    /// not a blanket ban on acting; it is a claim about provenance.
    #[test]
    fn allows_a_consequential_action_from_a_trusted_context() {
        let mut ifc = ifc(false);
        let allowed = ifc.decide(
            "s1",
            "ifcdemo__schedule_payment",
            &args(serde_json::json!({ "amount": 20, "payee": "landlord" })),
        );
        assert!(matches!(allowed, Decision::Allow { .. }), "{allowed:?}");
    }

    /// Taint cannot be laundered by following up with trusted reads, and it is
    /// not scoped to the offending call: once the planner has read adversarial
    /// text, every consequential action for the rest of the session is refused.
    #[test]
    fn untrusted_taint_is_permanent_within_a_session() {
        let mut ifc = ifc(false);
        ifc.decide("s1", "ifcdemo__read_inbox", &args(serde_json::json!({})));
        // A trusted read afterwards must not wash it out.
        ifc.decide("s1", "ifcdemo__read_secret", &args(serde_json::json!({})));

        for tool in ["ifcdemo__schedule_payment", "ifcdemo__publish"] {
            let decision = ifc.decide("s1", tool, &args(serde_json::json!({ "message": "hi" })));
            assert!(
                matches!(decision, Decision::Block { .. }),
                "{tool} should stay blocked: {decision:?}"
            );
        }

        // A different session is unaffected.
        let clean = ifc.decide(
            "s2",
            "ifcdemo__schedule_payment",
            &args(serde_json::json!({ "amount": 1 })),
        );
        assert!(matches!(clean, Decision::Allow { .. }), "{clean:?}");
    }

    /// Hiding is what buys the utility back, and it does so on the integrity
    /// axis as much as the confidentiality one.
    ///
    /// The injected text never reaches the planner, so the context stays
    /// trusted and consequential tools keep working — while the adversary's
    /// instruction has no way to influence anything, because the model never
    /// read it.
    #[test]
    fn hiding_an_untrusted_read_keeps_the_context_trusted() {
        let mut ifc = ifc(true);

        let decision = ifc.decide("s1", "ifcdemo__read_inbox", &args(serde_json::json!({})));
        let hide_ref = match decision {
            Decision::Hide { hide_ref, .. } => hide_ref,
            other => panic!("expected Hide for an untrusted source, got {other:?}"),
        };

        assert_eq!(
            ifc.sessions["s1"].context.integrity(),
            Integrity::Trusted,
            "a hidden read must not taint the context"
        );

        let allowed = ifc.decide(
            "s1",
            "ifcdemo__schedule_payment",
            &args(serde_json::json!({ "amount": 20, "payee": "landlord" })),
        );
        assert!(matches!(allowed, Decision::Allow { .. }), "{allowed:?}");

        // And the paper's Task 1: forwarding the withheld content onward is
        // permitted, because P-T does not require trusted *arguments* — only a
        // trusted decision. The inbox is public, so P-F is satisfied too.
        let forwarded = ifc.decide(
            "s1",
            "ifcdemo__publish",
            &args(serde_json::json!({ "message": hide_ref })),
        );
        assert!(
            matches!(forwarded, Decision::Allow { .. }),
            "forwarding untrusted-but-public content should be permitted: {forwarded:?}"
        );
    }

    /// The two axes do not collapse into one another. A trusted secret restricts
    /// egress while leaving actions available; an untrusted public message does
    /// exactly the reverse.
    #[test]
    fn the_axes_are_enforced_independently() {
        // Confidential but trusted: publishing is refused, acting is not. The
        // user wrote their own secret, so it remains sound grounds for a
        // decision even though it may not be disclosed.
        let mut secrets = ifc(false);
        secrets.decide("s", "ifcdemo__read_secret", &args(serde_json::json!({})));
        assert!(matches!(
            secrets.decide(
                "s",
                "ifcdemo__publish",
                &args(serde_json::json!({ "message": "x" }))
            ),
            Decision::Block { .. }
        ));
        assert!(
            matches!(
                secrets.decide(
                    "s",
                    "ifcdemo__schedule_payment",
                    &args(serde_json::json!({ "amount": 1 }))
                ),
                Decision::Allow { .. }
            ),
            "reading a trusted secret must not disable consequential actions"
        );

        // Public but untrusted: the reverse pairing. Egress is unrestricted, yet
        // every consequential action is refused.
        let mut inbox = ifc(false);
        inbox.decide("s", "ifcdemo__read_inbox", &args(serde_json::json!({})));
        let session = &inbox.sessions["s"];
        assert!(
            session
                .check_egress(&public(), &serde_json::json!({}))
                .is_ok(),
            "untrusted input must not restrict confidentiality"
        );
        assert!(matches!(
            inbox.decide(
                "s",
                "ifcdemo__schedule_payment",
                &args(serde_json::json!({ "amount": 1 }))
            ),
            Decision::Block { .. }
        ));
    }

    // ------------------------------------------------------------- switch ----

    #[test]
    fn disabled_by_default_allows_everything() {
        let mut ifc = Ifc::default();
        assert!(!ifc.is_enabled());
        for tool in [
            "ifcdemo__read_secret",
            "ifcdemo__read_inbox",
            "ifcdemo__publish",
            "ifcdemo__schedule_payment",
        ] {
            let d = ifc.decide("s1", tool, &args(serde_json::json!({ "message": "x" })));
            assert!(matches!(d, Decision::Allow { .. }), "{tool}: {d:?}");
        }
    }

    /// Tools outside the policy table are neutral: no tracking, no gate. This
    /// documents the demo's remaining fail-open, described on [`policy_for`].
    #[test]
    fn unknown_tools_are_neutral() {
        let mut ifc = ifc(true);
        let d = ifc.decide(
            "s1",
            "developer__shell",
            &args(serde_json::json!({ "cmd": "ls" })),
        );
        assert!(matches!(d, Decision::Allow { .. }), "{d:?}");
        assert_eq!(ifc.sessions["s1"].context.integrity(), Integrity::Trusted);
    }
}
