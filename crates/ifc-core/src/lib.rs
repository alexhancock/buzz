//! Generic information-flow control over reader-set confidentiality labels.
//!
//! This crate knows nothing about Buzz, Nostr, etc., and should stay that way.
//!
//! ```
//! use std::collections::BTreeSet;
//! use ifc_core::{ConfidentialityLabel, EgressError, FlowState};
//!
//! let universe = "example";
//! let private = ConfidentialityLabel::restricted(
//!     universe,
//!     BTreeSet::from(["alice", "bob"]),
//! )?;
//! let alice_only = ConfidentialityLabel::restricted_to(universe, "alice");
//! let public = ConfidentialityLabel::public(universe);
//! let mut flow = FlowState::default();
//! flow.observe(&private);
//!
//! assert_eq!(flow.check_egress(&alice_only), Ok(()));
//! assert_eq!(
//!     flow.check_egress(&public),
//!     Err(EgressError::DestinationWidensReaders),
//! );
//! # Ok::<(), ifc_core::LabelError>(())
//! ```

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};

/// The people or systems allowed to learn a value within one universe.
///
/// `Everyone` is for public information. `Only` holds the set of
/// principals allowed to read restricted information. Sending information from
/// one reader set to another is safe only when the destination adds no new
/// readers. For example, information readable by Alice and Bob may be narrowed
/// to Alice, but it must not be widened to Alice, Bob, and Carol.
///
/// When a computation combines inputs, their reader sets are intersected so
/// its output is restricted to principals allowed to read every input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReaderSet<Principal> {
    /// Every principal in the universe may read the value.
    Everyone,
    /// Only these principals may read the value.
    Only(BTreeSet<Principal>),
}

impl<Principal: Ord> ReaderSet<Principal> {
    /// Whether information readable by `self` may flow to `destination`.
    pub fn can_flow_to(&self, destination: &Self) -> bool {
        match (self, destination) {
            (Self::Everyone, _) => true,
            (Self::Only(_), Self::Everyone) => false,
            (Self::Only(source), Self::Only(destination)) => destination.is_subset(source),
        }
    }

    /// Combine the restrictions of two contributing inputs.
    ///
    /// A derived value may be read only by principals authorized for both
    /// inputs, so explicit reader sets are intersected. `Everyone` adds no
    /// restriction.
    pub fn join(&self, other: &Self) -> Self
    where
        Principal: Clone,
    {
        match (self, other) {
            (Self::Everyone, value) | (value, Self::Everyone) => value.clone(),
            (Self::Only(left), Self::Only(right)) => {
                Self::Only(left.intersection(right).cloned().collect())
            }
        }
    }

    /// Return the greatest label that can flow to both inputs.
    pub fn meet(&self, other: &Self) -> Self
    where
        Principal: Clone,
    {
        match (self, other) {
            (Self::Everyone, _) | (_, Self::Everyone) => Self::Everyone,
            (Self::Only(left), Self::Only(right)) => {
                Self::Only(left.union(right).cloned().collect())
            }
        }
    }

    /// Return the explicit readers, or `None` when everyone may read the value.
    pub fn explicit_readers(&self) -> Option<&BTreeSet<Principal>> {
        match self {
            Self::Everyone => None,
            Self::Only(readers) => Some(readers),
        }
    }

    /// Return the explicit number of readers, or `None` for public data.
    pub fn explicit_count(&self) -> Option<usize> {
        self.explicit_readers().map(BTreeSet::len)
    }
}

/// A reader-set confidentiality label inside one isolated universe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfidentialityLabel<Universe, Principal> {
    universe: Universe,
    readers: ReaderSet<Principal>,
}

impl<Universe, Principal> ConfidentialityLabel<Universe, Principal> {
    /// Label a value readable by every principal in `universe`.
    pub fn public(universe: Universe) -> Self {
        Self {
            universe,
            readers: ReaderSet::Everyone,
        }
    }

    /// Label a value with an explicit non-empty reader set.
    pub fn restricted(
        universe: Universe,
        readers: BTreeSet<Principal>,
    ) -> Result<Self, LabelError> {
        if readers.is_empty() {
            return Err(LabelError::EmptyReaderSet);
        }
        Ok(Self {
            universe,
            readers: ReaderSet::Only(readers),
        })
    }

    /// Label a value for exactly one principal.
    pub fn restricted_to(universe: Universe, principal: Principal) -> Self
    where
        Principal: Ord,
    {
        Self {
            universe,
            readers: ReaderSet::Only(BTreeSet::from([principal])),
        }
    }

    /// Return the universe in which this label is meaningful.
    pub fn universe(&self) -> &Universe {
        &self.universe
    }

    /// Return the authorized reader set.
    pub fn reader_set(&self) -> &ReaderSet<Principal> {
        &self.readers
    }

    /// Whether every principal in the universe may read the value.
    pub fn is_public(&self) -> bool {
        matches!(self.readers, ReaderSet::Everyone)
    }

    /// Return the explicit number of readers, or `None` for public data.
    pub fn reader_count(&self) -> Option<usize>
    where
        Principal: Ord,
    {
        self.readers.explicit_count()
    }
}

impl<Universe: Clone + Eq, Principal: Clone + Ord> ConfidentialityLabel<Universe, Principal> {
    /// Whether information with this label may flow to `destination`.
    pub fn can_flow_to(&self, destination: &Self) -> bool {
        self.universe == destination.universe && self.readers.can_flow_to(&destination.readers)
    }

    /// Combine the influence of two inputs.
    pub fn join(&self, other: &Self) -> Result<Self, LabelError> {
        if self.universe != other.universe {
            return Err(LabelError::CrossUniverse);
        }
        Ok(Self {
            universe: self.universe.clone(),
            readers: self.readers.join(&other.readers),
        })
    }
}

/// A confidentiality label violates the reader-set lattice invariants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelError {
    /// Restricted information must name at least one authorized reader.
    EmptyReaderSet,
    /// Labels from different universes cannot be combined.
    CrossUniverse,
}

impl Display for LabelError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyReaderSet => {
                formatter.write_str("restricted label has no authorized readers")
            }
            Self::CrossUniverse => formatter.write_str("labels belong to different universes"),
        }
    }
}

impl Error for LabelError {}

/// Monotonic confidentiality state for one computation boundary.
///
/// Every admitted label is joined into the accumulated label. Unknown or
/// cross-universe input permanently prevents ordinary egress. The state is not
/// cloneable because a caller must not retain a clean copy and later use it to
/// forget observed input.
///
/// ```compile_fail
/// let state = ifc_core::FlowState::<String, String>::default();
/// let _clean_copy = state.clone();
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct FlowState<Universe, Principal> {
    accumulated: Option<ConfidentialityLabel<Universe, Principal>>,
    unresolved_input: bool,
}

impl<Universe, Principal> Default for FlowState<Universe, Principal> {
    fn default() -> Self {
        Self {
            accumulated: None,
            unresolved_input: false,
        }
    }
}

impl<Universe: Clone + Eq, Principal: Clone + Ord> FlowState<Universe, Principal> {
    /// Record a labeled input that entered the computation.
    pub fn observe(&mut self, label: &ConfidentialityLabel<Universe, Principal>) {
        self.accumulated = match self.accumulated.take() {
            None => Some(label.clone()),
            Some(existing) => match existing.join(label) {
                Ok(combined) => Some(combined),
                Err(LabelError::CrossUniverse | LabelError::EmptyReaderSet) => {
                    self.unresolved_input = true;
                    Some(existing)
                }
            },
        };
    }

    /// Permanently record input whose label could not be established.
    pub fn mark_unknown(&mut self) {
        self.unresolved_input = true;
    }

    /// Check whether accumulated information may flow to `destination`.
    pub fn check_egress(
        &self,
        destination: &ConfidentialityLabel<Universe, Principal>,
    ) -> Result<(), EgressError> {
        if self.unresolved_input {
            return Err(EgressError::UnresolvedInput);
        }
        let Some(accumulated) = &self.accumulated else {
            return Ok(());
        };
        if accumulated.universe() != destination.universe() {
            return Err(EgressError::DestinationUniverseMismatch);
        }
        if !accumulated
            .reader_set()
            .can_flow_to(destination.reader_set())
        {
            return Err(EgressError::DestinationWidensReaders);
        }
        Ok(())
    }

    /// Whether any labeled input has entered the computation.
    pub fn has_observed_input(&self) -> bool {
        self.accumulated.is_some()
    }

    /// Return the label accumulated from all observed inputs.
    pub fn accumulated_label(&self) -> Option<&ConfidentialityLabel<Universe, Principal>> {
        self.accumulated.as_ref()
    }

    /// Whether unknown or cross-universe input has entered the computation.
    pub fn has_unresolved_input(&self) -> bool {
        self.unresolved_input
    }

    /// Capture state for detecting changes before a checked sink executes.
    pub fn snapshot(&self) -> FlowSnapshot<Universe, Principal> {
        FlowSnapshot {
            accumulated: self.accumulated.clone(),
            unresolved_input: self.unresolved_input,
        }
    }
}

/// An inert copy used only to detect changes before sink execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlowSnapshot<Universe, Principal> {
    accumulated: Option<ConfidentialityLabel<Universe, Principal>>,
    unresolved_input: bool,
}

/// Why accumulated information cannot use an ordinary egress path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EgressError {
    /// Some input had unknown provenance or belonged to another universe.
    UnresolvedInput,
    /// The destination belongs to a different confidentiality universe.
    DestinationUniverseMismatch,
    /// The destination introduces readers not authorized for every input.
    DestinationWidensReaders,
}

impl Display for EgressError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnresolvedInput => formatter.write_str("input provenance is unresolved"),
            Self::DestinationUniverseMismatch => {
                formatter.write_str("destination belongs to a different universe")
            }
            Self::DestinationWidensReaders => {
                formatter.write_str("destination widens the accumulated reader set")
            }
        }
    }
}

impl Error for EgressError {}

/// Whether information originated somewhere an adversary could have written.
///
/// This is the other half of the product lattice: confidentiality says who may
/// *learn* a value, integrity says whether a value may be allowed to *decide*
/// anything. They are independent. A user's own password is confidential and
/// trusted; a stranger's email is public and untrusted.
///
/// `Trusted` is the privileged end. Information may always be treated as less
/// trustworthy than it is, never as more, so a computation combining inputs is
/// trusted only when every input was.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Integrity {
    /// Written only by the user or the system acting on their behalf.
    #[default]
    Trusted,
    /// Could have been authored by an adversary.
    Untrusted,
}

impl Integrity {
    /// Combine the trustworthiness of two contributing inputs.
    ///
    /// One untrusted input is enough to make the result untrusted, so this is
    /// the lattice's least upper bound in the direction taint travels.
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Trusted, Self::Trusted) => Self::Trusted,
            _ => Self::Untrusted,
        }
    }

    pub fn is_trusted(self) -> bool {
        matches!(self, Self::Trusted)
    }
}

impl Display for Integrity {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trusted => formatter.write_str("trusted"),
            Self::Untrusted => formatter.write_str("untrusted"),
        }
    }
}

/// Monotonic integrity of one context — a conversation, a plan, a planner's
/// working memory.
///
/// Where [`FlowState`] accumulates the labels of data a computation has *read*,
/// this accumulates the trustworthiness of everything that has *influenced* it.
/// The distinction matters because the two are checked against different things:
/// accumulated confidentiality is checked against a destination, while
/// accumulated integrity is checked against nothing at all. It is a property of
/// the context alone, which is why [`check_trusted_action`] takes no argument.
///
/// Like `FlowState`, this is deliberately not cloneable: a caller must not keep
/// a pristine copy and later use it to pretend the context never read the email.
///
/// ```
/// use ifc_core::{Integrity, IntegrityState};
///
/// let mut context = IntegrityState::default();
/// // Nothing has influenced the context yet, so a consequential action is fine.
/// assert!(context.check_trusted_action().is_ok());
///
/// // An adversary's text enters the context.
/// context.observe(Integrity::Untrusted);
///
/// // Now no consequential action may be taken on its say-so, whatever its
/// // arguments look like.
/// assert!(context.check_trusted_action().is_err());
///
/// // And trusted input afterwards cannot wash it out.
/// context.observe(Integrity::Trusted);
/// assert!(context.check_trusted_action().is_err());
/// ```
///
/// ```compile_fail
/// let state = ifc_core::IntegrityState::default();
/// let _clean_copy = state.clone();
/// ```
#[derive(Debug, Default, Eq, PartialEq)]
pub struct IntegrityState {
    integrity: Integrity,
}

impl IntegrityState {
    /// Record input that has influenced this context.
    pub fn observe(&mut self, label: Integrity) {
        self.integrity = self.integrity.join(label);
    }

    /// The trustworthiness of everything that has influenced this context.
    pub fn integrity(&self) -> Integrity {
        self.integrity
    }

    /// The trusted-action policy (P-T): a consequential action may proceed only
    /// if the context that chose it was untainted by adversarial input.
    ///
    /// Note what is absent: the action's arguments. P-T is a claim about who
    /// decided to act, not about what is being sent. Forwarding an untrusted
    /// email body to its intended recipient satisfies P-T, because the user
    /// asked for it; scheduling a payment named in that email does not, because
    /// the email asked for it.
    pub fn check_trusted_action(&self) -> Result<(), IntegrityError> {
        if self.integrity.is_trusted() {
            Ok(())
        } else {
            Err(IntegrityError::UntrustedContext)
        }
    }
}

/// Why a consequential action may not proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityError {
    /// Adversarial input had entered the context that chose the action.
    UntrustedContext,
}

impl Display for IntegrityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UntrustedContext => {
                formatter.write_str("action was chosen in an untrusted context")
            }
        }
    }
}

impl Error for IntegrityError {}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn readers(mask: u8) -> BTreeSet<u8> {
        (0..8).filter(|bit| mask & (1 << bit) != 0).collect()
    }

    fn label(mask: u8) -> ConfidentialityLabel<u8, u8> {
        ConfidentialityLabel::restricted(1, readers(mask.max(1))).expect("non-empty readers")
    }

    #[derive(Clone, Copy, Debug)]
    enum Audience {
        Public,
        Restricted(u8),
    }

    impl Audience {
        fn label(self) -> ConfidentialityLabel<u8, u8> {
            match self {
                Self::Public => ConfidentialityLabel::public(1),
                Self::Restricted(mask) => label(mask),
            }
        }

        fn can_flow_to(self, destination: Self) -> bool {
            match (self, destination) {
                (Self::Public, _) => true,
                (Self::Restricted(_), Self::Public) => false,
                (Self::Restricted(source), Self::Restricted(destination)) => {
                    destination & !source == 0
                }
            }
        }
    }

    fn audience_strategy() -> impl Strategy<Value = Audience> {
        prop_oneof![
            Just(Audience::Public),
            (1_u8..=u8::MAX).prop_map(Audience::Restricted),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Checks reader-set inclusion against an independent bit-set model and
        /// verifies that join is commutative, associative, idempotent, and
        /// admits exactly the destinations admitted by both inputs. This catches
        /// a reversed subset check, union in place of intersection, and special
        /// handling of public data that accidentally widens readers.
        #[test]
        fn reader_sets_obey_flow_and_join_laws(
            a in audience_strategy(),
            b in audience_strategy(),
            c in audience_strategy(),
            destination in audience_strategy(),
        ) {
            let a_label = a.label();
            let b_label = b.label();
            let c_label = c.label();
            let destination_label = destination.label();

            prop_assert_eq!(
                a_label.can_flow_to(&destination_label),
                a.can_flow_to(destination),
            );
            prop_assert!(ConfidentialityLabel::public(1).can_flow_to(&a_label));
            if !a_label.is_public() {
                prop_assert!(!a_label.can_flow_to(&ConfidentialityLabel::public(1)));
            }

            let ab = a_label.join(&b_label).expect("same universe");
            prop_assert_eq!(&ab, &b_label.join(&a_label).expect("same universe"));
            prop_assert_eq!(
                a_label.join(&a_label).expect("same universe"),
                a_label.clone(),
            );
            prop_assert_eq!(
                a_label
                    .join(&b_label.join(&c_label).expect("same universe"))
                    .expect("same universe"),
                ab.join(&c_label).expect("same universe"),
            );
            prop_assert_eq!(
                ab.can_flow_to(&destination_label),
                a_label.can_flow_to(&destination_label)
                    && b_label.can_flow_to(&destination_label),
            );
        }
    }

    /// Checks the two absorption laws linking join and meet. These catch a
    /// locally plausible implementation where each operation works alone but
    /// they do not form one consistent lattice.
    #[test]
    fn reader_set_join_and_meet_satisfy_absorption() {
        let values = [
            ReaderSet::Everyone,
            ReaderSet::Only(readers(0b0001)),
            ReaderSet::Only(readers(0b0010)),
            ReaderSet::Only(readers(0b0011)),
        ];

        for left in &values {
            for right in &values {
                assert_eq!(left.join(&left.meet(right)), *left);
                assert_eq!(left.meet(&left.join(right)), *left);
            }
        }
    }

    /// Checks that every observed input permanently restricts later egress and
    /// that unknown provenance cannot be cleared. This catches taint rollback
    /// and mistakenly replacing an accumulated label instead of joining it.
    #[test]
    fn flow_state_accumulates_restrictions_and_never_forgets_unknown_input() {
        let mut state = FlowState::default();
        assert_eq!(state.check_egress(&label(0b0011)), Ok(()));
        state.observe(&label(0b0011));
        state.observe(&label(0b0110));

        assert_eq!(state.accumulated_label(), Some(&label(0b0010)));
        assert_eq!(state.check_egress(&label(0b0010)), Ok(()));
        assert_eq!(
            state.check_egress(&label(0b0011)),
            Err(EgressError::DestinationWidensReaders)
        );

        state.mark_unknown();
        assert_eq!(
            state.check_egress(&label(0b0010)),
            Err(EgressError::UnresolvedInput)
        );
    }

    /// Checks that an egress destination in another universe is distinguished
    /// from a destination that widens the reader set.
    #[test]
    fn cross_universe_destination_reports_universe_mismatch() {
        let mut state = FlowState::default();
        state.observe(&label(0b0011));
        let destination =
            ConfidentialityLabel::restricted(2, readers(0b0001)).expect("non-empty readers");

        assert_eq!(
            state.check_egress(&destination),
            Err(EgressError::DestinationUniverseMismatch)
        );
    }

    /// Checks that combining labels from distinct universes fails closed
    /// for all later output. This catches accidental comparison of otherwise
    /// identical reader identifiers across unrelated confidentiality universes.
    #[test]
    fn cross_universe_input_permanently_blocks_egress() {
        let mut state = FlowState::default();
        state.observe(&label(0b0011));
        state.observe(
            &ConfidentialityLabel::restricted(2, readers(0b0011)).expect("non-empty readers"),
        );

        assert!(state.has_unresolved_input());
        assert_eq!(
            state.check_egress(&label(0b0001)),
            Err(EgressError::UnresolvedInput)
        );
    }

    #[test]
    fn integrity_join_is_untrusted_unless_every_input_is_trusted() {
        use Integrity::{Trusted, Untrusted};

        assert_eq!(Trusted.join(Trusted), Trusted);
        assert_eq!(Trusted.join(Untrusted), Untrusted);
        assert_eq!(Untrusted.join(Trusted), Untrusted);
        assert_eq!(Untrusted.join(Untrusted), Untrusted);
    }

    /// The property that makes P-T worth anything: taint cannot be laundered.
    /// An attacker who gets one untrusted byte into a context must not be able
    /// to clear it by following up with any amount of trusted input.
    #[test]
    fn integrity_is_monotonic() {
        let mut context = IntegrityState::default();
        assert_eq!(context.integrity(), Integrity::Trusted);

        context.observe(Integrity::Untrusted);
        for _ in 0..100 {
            context.observe(Integrity::Trusted);
        }

        assert_eq!(context.integrity(), Integrity::Untrusted);
        assert_eq!(
            context.check_trusted_action(),
            Err(IntegrityError::UntrustedContext)
        );
    }

    /// Integrity and confidentiality are orthogonal, and the demo's two sources
    /// sit on opposite diagonals: a private value the user wrote is confidential
    /// but trusted, while a stranger's public message is untrusted but carries
    /// no secrecy obligation at all.
    #[test]
    fn the_two_axes_are_independent() {
        let own_secret = (label(0b0001), Integrity::Trusted);
        let strangers_email = (
            ConfidentialityLabel::<u8, u8>::public(1),
            Integrity::Untrusted,
        );

        // Confidentiality differs...
        assert!(!own_secret.0.is_public());
        assert!(strangers_email.0.is_public());
        // ...and integrity differs in the opposite direction.
        assert!(own_secret.1.is_trusted());
        assert!(!strangers_email.1.is_trusted());

        // So reading the public email leaves egress wide open,
        let mut flow = FlowState::default();
        flow.observe(&strangers_email.0);
        assert_eq!(
            flow.check_egress(&ConfidentialityLabel::<u8, u8>::public(1)),
            Ok(())
        );
        // while still forbidding every consequential action. This is the case
        // a confidentiality-only system cannot see.
        let mut context = IntegrityState::default();
        context.observe(strangers_email.1);
        assert!(context.check_trusted_action().is_err());
    }

    proptest! {
        /// Integrity never recovers, whatever order input arrives in: the
        /// accumulated value is untrusted exactly when some input was.
        #[test]
        fn integrity_accumulates_independent_of_order(inputs: Vec<bool>) {
            let mut context = IntegrityState::default();
            for untrusted in &inputs {
                context.observe(if *untrusted {
                    Integrity::Untrusted
                } else {
                    Integrity::Trusted
                });
            }

            let any_untrusted = inputs.iter().any(|untrusted| *untrusted);
            prop_assert_eq!(context.integrity() == Integrity::Untrusted, any_untrusted);
            prop_assert_eq!(context.check_trusted_action().is_err(), any_untrusted);
        }
    }
}
