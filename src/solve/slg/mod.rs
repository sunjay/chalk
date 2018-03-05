//! An alternative solver based around the SLG algorithm, which
//! implements the well-formed semantics. This algorithm is very
//! closed based on the description found in the following paper,
//! which I will refer to in the comments as EWFS:
//!
//! > Efficient Top-Down Computation of Queries Under the Well-formed Semantics
//! > (Chen, Swift, and Warren; Journal of Logic Programming '95)
//!
//! However, to understand that paper, I would recommend first
//! starting with the following paper, which I will refer to in the
//! comments as NFTD:
//!
//! > A New Formulation of Tabled resolution With Delay
//! > (Swift; EPIA '99)
//!
//! In addition, I incorporated extensions from the following papers,
//! which I will refer to as SA and RR respectively, that
//! describes how to do introduce approximation when processing
//! subgoals and so forth:
//!
//! > Terminating Evaluation of Logic Programs with Finite Three-Valued Models
//! > Riguzzi and Swift; ACM Transactions on Computational Logic 2013
//! > (Introduces "subgoal abstraction", hence the name SA)
//! >
//! > Radial Restraint
//! > Grosof and Swift; 2013
//!
//! Another useful paper that gives a kind of high-level overview of
//! concepts at play is the following, which I will refer to as XSB:
//!
//! > XSB: Extending Prolog with Tabled Logic Programming
//! > (Swift and Warren; Theory and Practice of Logic Programming '10)
//!
//! While this code is adapted from the algorithms described in those
//! papers, it is not the same. For one thing, the approaches there
//! had to be extended to our context, and in particular to coping
//! with hereditary harrop predicates and our version of unification
//! (which produces subgoals). I believe those to be largely faithful
//! extensions. However, there are some other places where I
//! intentionally dieverged from the semantics as described in the
//! papers -- e.g. by more aggressively approximating -- which I
//! marked them with a comment DIVERGENCE. Those places may want to be
//! evaluated in the future.
//!
//! Glossary of other terms:
//!
//! - WAM: Warren abstract machine, an efficient way to evaluate Prolog programs.
//!   See <http://wambook.sourceforge.net/>.
//! - HH: Hereditary harrop predicates. What Chalk deals in.
//!   Popularized by Lambda Prolog.

use ir::*;
use stacker;
use std::collections::HashSet;
use std::cmp::min;
use std::hash::{Hash, Hasher};
use std::mem;
use std::usize;

crate mod forest;

mod aggregate;
crate mod context;
use self::context::Context;
mod logic;
mod simplify;
mod stack;
mod strand;
mod table;
mod tables;
mod test;

index_struct! {
    struct TableIndex {
        value: usize,
    }
}

/// The StackIndex identifies the position of a table's goal in the
/// stack of goals that are actively being processed. Note that once a
/// table is completely evaluated, it may be popped from the stack,
/// and hence no longer have a stack index.
index_struct! {
    struct StackIndex {
        value: usize,
    }
}

/// The `DepthFirstNumber` (DFN) is a sequential number assigned to
/// each goal when it is first encountered. The naming (taken from
/// EWFS) refers to the idea that this number tracks the index of when
/// we encounter the goal during a depth-first traversal of the proof
/// tree.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DepthFirstNumber {
    value: u64,
}

copy_fold!(DepthFirstNumber);

/// The paper describes these as `A :- D | G`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
crate struct ExClause<C: Context> {
    /// The substitution which, applied to the goal of our table,
    /// would yield A.
    subst: Substitution,

    /// Delayed literals: things that we depend on negatively,
    /// but which have not yet been fully evaluated.
    delayed_literals: Vec<DelayedLiteral>,

    /// Region constraints we have accumulated.
    constraints: Vec<InEnvironment<Constraint>>,

    /// Subgoals: literals that must be proven
    subgoals: Vec<Literal<C>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SimplifiedAnswers {
    answers: Vec<SimplifiedAnswer>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SimplifiedAnswer {
    /// A fully instantiated version of the goal for which the query
    /// is true (including region constraints).
    subst: CanonicalConstrainedSubst,

    /// If this flag is set, then the answer could be neither proven
    /// nor disproven. In general, the existence of a non-empty set of
    /// delayed literals simply means the answer's status is UNKNOWN,
    /// either because the size of the answer exceeded `max_size` or
    /// because of a negative loop (e.g., `P :- not { P }`).
    ambiguous: bool,
}

#[derive(Clone, Debug)]
enum DelayedLiteralSets {
    /// Corresponds to a single, empty set.
    None,

    /// Some (non-zero) number of non-empty sets.
    Some(HashSet<DelayedLiteralSet>),
}

/// A set of delayed literals. The vector in this struct must
/// be sorted, ensuring that we don't have to worry about permutations.
///
/// (One might expect delayed literals to always be ground, since
/// non-ground negative literals result in flounded
/// executions. However, due to the approximations introduced via RR
/// to ensure termination, it *is* in fact possible for delayed goals
/// to contain free variables. For example, what could happen is that
/// we get back an approximated answer with `Goal::CannotProve` as a
/// delayed literal, which in turn forces its subgoal to be delayed,
/// and so forth. Therefore, we store canonicalized goals.)
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
struct DelayedLiteralSet {
    delayed_literals: Vec<DelayedLiteral>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum DelayedLiteral {
    /// Something which can never be proven nor disproven. Inserted
    /// when truncation triggers; doesn't arise normally.
    CannotProve(()),

    /// We are blocked on a negative literal `~G`, where `G` is the
    /// goal of the given table. Because negative goals must always be
    /// ground, we don't need any other information.
    Negative(TableIndex),

    /// We are blocked on a positive literal `Li`; we found a
    /// **conditional** answer (the `CanonicalConstrainedSubst`) within the
    /// given table, but we have to come back later and see whether
    /// that answer turns out to be true.
    Positive(TableIndex, CanonicalConstrainedSubst),
}

enum_fold!(DelayedLiteral[] { CannotProve(a), Negative(a), Positive(a, b) });

/// Either `A` or `~A`, where `A` is a `Env |- Goal`.
#[derive(Clone, Debug)]
enum Literal<C: Context> {
    Positive(C::GoalInEnvironment),
    Negative(C::GoalInEnvironment),
}

impl<C: Context> PartialEq for Literal<C> {
    fn eq(&self, other: &Literal<C>) -> bool {
        match (self, other) {
            (Literal::Positive(goal1), Literal::Positive(goal2))
            | (Literal::Negative(goal1), Literal::Negative(goal2)) => goal1 == goal2,

            _ => false,
        }
    }
}

impl<C: Context> Eq for Literal<C> {
}

impl<C: Context> Hash for Literal<C> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        mem::discriminant(self).hash(state);
        match self {
            Literal::Positive(goal) | Literal::Negative(goal) => {
                goal.hash(state);
            }
        }
    }
}

/// The `Minimums` structure is used to track the dependencies between
/// some item E on the evaluation stack. In particular, it tracks
/// cases where the success of E depends (or may depend) on items
/// deeper in the stack than E (i.e., with lower DFNs).
///
/// `positive` tracks the lowest index on the stack to which we had a
/// POSITIVE dependency (e.g. `foo(X) :- bar(X)`) -- meaning that in
/// order for E to succeed, the dependency must succeed. It is
/// initialized with the index of the predicate on the stack. So
/// imagine we have a stack like this:
///
///     // 0 foo(X)   <-- bottom of stack
///     // 1 bar(X)
///     // 2 baz(X)   <-- top of stack
///
/// In this case, `positive` would be initially 0, 1, and 2 for `foo`,
/// `bar`, and `baz` respectively. This reflects the fact that the
/// answers for `foo(X)` depend on the answers for `foo(X)`. =)
///
/// Now imagine that we had a clause `baz(X) :- foo(X)`, inducing a
/// cycle. In this case, we would update `positive` for `baz(X)` to be
/// 0, reflecting the fact that its answers depend on the answers for
/// `foo(X)`. Similarly, the minimum for `bar` would (eventually) be
/// updated, since it too transitively depends on `foo`. `foo` is
/// unaffected.
///
/// `negative` tracks the lowest index on the stack to which we had a
/// NEGATIVE dependency (e.g., `foo(X) :- not { bar(X) }`) -- meaning
/// that for E to succeed, the dependency must fail. This is initially
/// `usize::MAX`, reflecting the fact that the answers for `foo(X)` do
/// not depend on `not(foo(X))`. When negative cycles are encountered,
/// however, this value must be updated.
#[derive(Copy, Clone, Debug)]
struct Minimums {
    positive: DepthFirstNumber,
    negative: DepthFirstNumber,
}

#[derive(Copy, Clone, Debug)]
crate enum Satisfiable<T> {
    Yes(T),
    No,
}

type CanonicalConstrainedSubst = Canonical<ConstrainedSubst>;
type CanonicalGoal<D> = Canonical<InEnvironment<Goal<D>>>;
type UCanonicalGoal<D> = UCanonical<InEnvironment<Goal<D>>>;

impl DelayedLiteralSets {
    fn is_empty(&self) -> bool {
        match *self {
            DelayedLiteralSets::None => true,
            DelayedLiteralSets::Some(_) => false,
        }
    }
}

impl DelayedLiteralSet {
    fn is_empty(&self) -> bool {
        self.delayed_literals.is_empty()
    }

    fn is_subset(&self, other: &DelayedLiteralSet) -> bool {
        self.delayed_literals
            .iter()
            .all(|elem| other.delayed_literals.binary_search(elem).is_ok())
    }
}

impl Minimums {
    const MAX: Minimums = Minimums {
        positive: DepthFirstNumber::MAX,
        negative: DepthFirstNumber::MAX,
    };

    /// Update our fields to be the minimum of our current value
    /// and the values from other.
    fn take_minimums(&mut self, other: &Minimums) {
        self.positive = min(self.positive, other.positive);
        self.negative = min(self.negative, other.negative);
    }

    fn minimum_of_pos_and_neg(&self) -> DepthFirstNumber {
        min(self.positive, self.negative)
    }
}

impl DepthFirstNumber {
    const MIN: DepthFirstNumber = DepthFirstNumber { value: 0 };
    const MAX: DepthFirstNumber = DepthFirstNumber {
        value: ::std::u64::MAX,
    };

    fn next(&mut self) -> DepthFirstNumber {
        let value = self.value;
        assert!(value < ::std::u64::MAX);
        self.value += 1;
        DepthFirstNumber { value }
    }
}

/// Because we recurse so deeply, we rely on stacker to
/// avoid overflowing the stack.
fn maybe_grow_stack<F, R>(op: F) -> R
where
    F: FnOnce() -> R,
{
    // These numbers are somewhat randomly chosen to make tests work
    // well enough on my system. In particular, because we only test
    // for growing the stack in `new_clause`, a red zone of 32K was
    // insufficient to prevent stack overflow. - nikomatsakis
    stacker::maybe_grow(256 * 1024, 2 * 1024 * 1024, op)
}
