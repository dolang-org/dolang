//! Trivial protocols.
//!
//! A protocol is trivial when every member it requires is one `Value` declares
//! and its ancestry names no class but `Value`. Every value has those members,
//! so conformance by members alone would admit every value, top included, and
//! constrain nothing. A trivial protocol is therefore nominal: a class is below
//! it only by naming it. That keeps sound the solver's rule that top is outside
//! everything narrower than it.
//!
//! A protocol requires its public members and its ancestors', except static
//! members and `(init)`, as conformance does. Member types are not compared:
//! a protocol is trivial by the names it requires alone.

use rustc_hash::FxHashSet;

use super::{Database, DeclId, DeclKind, Intrinsic, Member, MemberKey, Scope};

impl Database {
    /// Whether a member is one a protocol requires of a conforming declaration
    pub(crate) fn required_member(&self, key: MemberKey, member: &Member) -> bool {
        !key.private
            && member.scope() != Scope::Static
            && !(key.special && key.name == self.intern_symbol("init"))
    }

    /// Whether `decl` is a protocol whose required members `Value` declares
    /// and whose ancestry names no class but `Value`. False when an ancestor
    /// isn't a declaration.
    pub(crate) fn trivial_protocol(&self, decl: DeclId) -> bool {
        if self.declaration(decl).source.kind != DeclKind::Protocol {
            return false;
        }
        // Without `Value`, only a protocol requiring nothing is trivial
        let value = self
            .intrinsic_decl(Intrinsic::Value)
            .map(|value| self.declaration(value));
        let declared = |key: MemberKey, scope: Scope| {
            value.is_some_and(|value| {
                value
                    .members
                    .iter()
                    .any(|(found, member)| *found == key && member.scope() == scope)
            })
        };
        let mut seen = FxHashSet::default();
        let mut stack = vec![decl];
        while let Some(decl) = stack.pop() {
            if !seen.insert(decl) {
                continue;
            }
            let declaration = self.declaration(decl);
            if declaration.source.kind != DeclKind::Protocol {
                return false;
            }
            let redundant = declaration.members.iter().all(|(key, member)| {
                !self.required_member(*key, member) || declared(*key, member.scope())
            });
            if !redundant {
                return false;
            }
            for supertype in &declaration.supertypes {
                match self.supertype_decl(supertype.ty) {
                    Ok(Some(head)) => stack.push(head),
                    Ok(None) => {}
                    Err(_) => return false,
                }
            }
        }
        true
    }
}
