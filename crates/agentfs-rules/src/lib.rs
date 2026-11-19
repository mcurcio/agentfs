//! The Rules domain (docs/trees.md, "Rules and the untracked area"): which
//! new names a tree records and which live in its untracked area. Pure:
//! built once from the operator's rule files, it reads nothing itself.

mod operator;
mod set;

pub use operator::{AttrState, OperatorRules};
pub use set::{Nested, NewKind, PathFacts, Placement, RuleSet, RuleTexts, RulesError, Untracked};
