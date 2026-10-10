//! Flow state: what each variable holds at a point, and the operand stack.

use std::collections::HashMap;

use crate::typeck::{cfg::Tag, r#type::TypeId};

/// What a variable holds at a point
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fact {
    /// The join of the values it may hold; bottom when it holds none
    pub(crate) ty: TypeId,
    /// Whether some path reaches the point without assigning it
    pub(crate) unassigned: bool,
}

/// The state at a point: a fact for each variable its function owns, by its slot,
/// and the operand stack
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct State {
    pub(super) vars: Vec<Fact>,
    /// By slot, whether some path has created a function its variable escapes
    /// through, so that assigning it joins its accumulator
    pub(super) escaped: Vec<bool>,
    pub(super) stack: Vec<TypeId>,
    /// Whether the top of the stack is a `Dup` of the slot below it, which a
    /// branch on it narrows too
    pub(super) dup: bool,
}

/// The `finally` tags a block is entered with, innermost last, identifying its state
/// among the others of the same block
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct CtxId(u32);

impl CtxId {
    /// Outside every `finally`
    pub(super) const ROOT: Self = Self(0);
}

/// The interned tag stacks
pub(super) struct Contexts {
    stacks: Vec<Vec<Tag>>,
    ids: HashMap<Vec<Tag>, CtxId>,
}

impl Contexts {
    pub(super) fn new() -> Self {
        let mut contexts = Self {
            stacks: Vec::new(),
            ids: HashMap::new(),
        };
        let root = contexts.intern(Vec::new());
        debug_assert_eq!(root, CtxId::ROOT);
        contexts
    }

    fn intern(&mut self, stack: Vec<Tag>) -> CtxId {
        if let Some(&id) = self.ids.get(&stack) {
            return id;
        }
        let id = CtxId(u32::try_from(self.stacks.len()).expect("too many contexts"));
        self.stacks.push(stack.clone());
        self.ids.insert(stack, id);
        id
    }

    pub(super) fn tags(&self, id: CtxId) -> &[Tag] {
        &self.stacks[id.0 as usize]
    }

    /// The context with `tag` entered inside `id`
    pub(super) fn push(&mut self, id: CtxId, tag: Tag) -> CtxId {
        let mut stack = self.stacks[id.0 as usize].clone();
        stack.push(tag);
        self.intern(stack)
    }

    /// The context of the `depth` outermost `finally` bodies of `id`'s
    pub(super) fn truncate(&mut self, id: CtxId, depth: u32) -> CtxId {
        let stack = &self.stacks[id.0 as usize];
        let depth = depth as usize;
        assert!(
            depth <= stack.len(),
            "an edge enters a `finally` without a tag"
        );
        if depth == stack.len() {
            return id;
        }
        let stack = stack[..depth].to_vec();
        self.intern(stack)
    }
}
