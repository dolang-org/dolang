//! Typing destructuring patterns by the schema `S` of the `Unpack[S]` a value
//! reaches.
//!
//! The pattern is matched against `S` as the runtime binds it. Positional items
//! are distributed by count: with `k` items, the first `k` slots are filled in
//! order, defaulted slots past them take their defaults, and items beyond every
//! slot go to a positional rest or are an error. Keyed slots take the items with
//! their key, and keyed items no slot takes go to a keyed rest or are an error.
//!
//! Every way of filling `S`'s multiplicities that the pattern accepts is a
//! possibility. A slot's type is the join over the possibilities, and the rest
//! unpacks as a tail schema admitting what each possibility leaves. A pattern
//! with no possibility is impossible. A possible pattern may still fail to
//! match a value, as `S` may admit other fillings too.

use std::collections::BTreeSet;

use super::{
    schema::{Opacity, Shape, Slot},
    *,
};

/// A destructuring pattern, as the walk sees it
pub(crate) struct PatternShape {
    /// Each positional slot, by whether it has a default. Defaulted slots follow
    /// the others.
    pub(crate) positional: Vec<bool>,
    /// Each keyed slot's key, and whether it has a default
    pub(crate) keyed: Vec<(TypeId, bool)>,
    /// Whether leftover positional items are accepted, by a rest
    pub(crate) positional_rest: bool,
    /// Whether leftover keyed items are accepted, by a rest
    pub(crate) keyed_rest: bool,
}

/// A pattern walked against a value's schemas
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unpacked {
    /// Each positional slot's type, then each keyed slot's. A slot that's never
    /// filled is bottom: its default gives its type.
    pub(crate) slots: Vec<TypeId>,
    /// What the pattern leaves, or `None` if that's unknown
    pub(crate) tail: Option<Tail>,
    /// Whether any possibility matches
    pub(crate) possible: bool,
}

/// The items a pattern leaves, by lane
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) positional: Vec<SchemaItem>,
    pub(crate) keyed: Vec<SchemaItem>,
}

/// A schema's items, reified and flattened
struct Atoms {
    positional: Vec<(Multiplicity, TypeId)>,
    keyed: Vec<(Multiplicity, TypeId, TypeId)>,
}

/// A lane's walk: its slots' types and its tail
type Lane = (Vec<TypeId>, Vec<SchemaItem>);

/// One member's walk
struct Walk {
    slots: Vec<TypeId>,
    tail: Tail,
}

impl Solver<'_> {
    /// Walk `pattern` against the schemas `ty`'s members unpack as, through
    /// `unpack`, the `Unpack` declaration. `None` when a member's schema can't be
    /// found: it isn't a class reaching `Unpack`, or its schema can't be exposed.
    pub(crate) fn unpack_pattern(
        &self,
        ty: TypeId,
        unpack: DeclId,
        pattern: &PatternShape,
    ) -> Option<Unpacked> {
        let count = pattern.positional.len() + pattern.keyed.len();
        let unknown = self.db.unknown();
        let mut walks = Vec::new();
        let mut dynamic = false;
        for member in self.union_members(ty) {
            let UnionMember::Type(member) = member else {
                return None;
            };
            if member == unknown {
                dynamic = true;
                continue;
            }
            let atoms = self.unpack_atoms(member, unpack).ok()??;
            if let Some(walk) = self.walk(&atoms, pattern).ok()? {
                walks.push(walk);
            }
        }
        if dynamic {
            return Some(Unpacked {
                slots: vec![unknown; count],
                tail: None,
                possible: true,
            });
        }
        let mut slots = vec![self.db.bottom(); count];
        for walk in &walks {
            for (slot, &ty) in slots.iter_mut().zip(&walk.slots) {
                *slot = self.lub(*slot, ty);
            }
        }
        let possible = !walks.is_empty();
        let tail = match &walks[..] {
            [] => Tail::default(),
            [walk] => Tail {
                positional: walk.tail.positional.clone(),
                keyed: walk.tail.keyed.clone(),
            },
            _ => self.join_tails(walks.iter().map(|walk| &walk.tail)),
        };
        Some(Unpacked {
            slots,
            tail: Some(tail),
            possible,
        })
    }

    /// The flattened schema a member unpacks as, `None` when it can't be found
    fn unpack_atoms(&self, member: TypeId, unpack: DeclId) -> Result<Option<Atoms>, Issue> {
        let Some(start) = self.start(member)? else {
            return Ok(None);
        };
        let Some(found) = self.ancestor(start, unpack, &mut HashSet::new(), 0)? else {
            return Ok(None);
        };
        let [schema] = found.arguments[..] else {
            return Ok(None);
        };
        let mut shape = Shape::default();
        let keep = HashSet::new();
        self.include(
            schema,
            Multiplicity::Required,
            0,
            Some(&keep),
            &mut shape,
            0,
        )?;
        self.atoms(&shape)
    }

    /// A shape's atoms, reified. The dynamic schema is a repeated item of
    /// `Unknown` in each lane it occupies.
    fn atoms(&self, shape: &Shape) -> Result<Option<Atoms>, Issue> {
        let unknown = self.db.unknown();
        let mut atoms = Atoms {
            positional: Vec::new(),
            keyed: Vec::new(),
        };
        let mut open_keyed = false;
        for slot in &shape.positional {
            match *slot {
                Slot::Atom(atom) => {
                    let ty = self.reify(atom.ty)?;
                    atoms.positional.push((atom.multiplicity, ty));
                }
                Slot::Opaque(index) => {
                    let opaque = shape.opaque[index];
                    if opaque.opacity != Opacity::Unknown {
                        return Ok(None);
                    }
                    if opaque.positional() {
                        atoms.positional.push((Multiplicity::Repeated, unknown));
                    }
                    open_keyed |= opaque.keyed();
                }
            }
        }
        for atom in &shape.keyed {
            let key = self.reify(atom.key)?;
            let value = self.reify(atom.value)?;
            atoms.keyed.push((atom.multiplicity, key, value));
        }
        if open_keyed {
            atoms.keyed.push((Multiplicity::Repeated, unknown, unknown));
        }
        Ok(Some(atoms))
    }

    /// Walk a pattern against one schema's atoms: `None` if it's impossible
    fn walk(&self, atoms: &Atoms, pattern: &PatternShape) -> Result<Option<Walk>, Issue> {
        let Some((mut slots, positional)) = self.walk_positional(&atoms.positional, pattern) else {
            return Ok(None);
        };
        let Some((keyed_slots, keyed)) = self.walk_keyed(&atoms.keyed, pattern)? else {
            return Ok(None);
        };
        slots.extend(keyed_slots);
        Ok(Some(Walk {
            slots,
            tail: Tail { positional, keyed },
        }))
    }

    /// The positional slots' types and the positional tail. Each state is the
    /// index of the next atom that can take an item.
    fn walk_positional(
        &self,
        atoms: &[(Multiplicity, TypeId)],
        pattern: &PatternShape,
    ) -> Option<Lane> {
        let slots = pattern.positional.len();
        let required = pattern.positional.iter().filter(|&&d| !d).count();
        // Whether every atom from a state on may take no item
        let ends = |state: usize| {
            atoms[state..]
                .iter()
                .all(|&(multiplicity, _)| multiplicity != Multiplicity::Required)
        };
        // The atoms that can take the next item from a state, and the states
        // that follow
        let steps = |state: usize| {
            let mut steps = Vec::new();
            for (index, &(multiplicity, _)) in atoms.iter().enumerate().skip(state) {
                let next = match multiplicity {
                    Multiplicity::Repeated => index,
                    _ => index + 1,
                };
                steps.push((index, next));
                if multiplicity == Multiplicity::Required {
                    break;
                }
            }
            steps
        };
        let mut forward: Vec<BTreeSet<usize>> = vec![BTreeSet::from([0])];
        for step in 0..slots {
            let next = (forward[step].iter())
                .flat_map(|&state| steps(state))
                .map(|(_, next)| next)
                .collect();
            forward.push(next);
        }
        // The states on a path the pattern accepts: with `k` items, where
        // defaults fill the slots past them or a rest takes what's left
        let mut alive: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); slots + 1];
        let mut accepted = Vec::new();
        for (count, states) in forward.iter().enumerate().skip(required) {
            for &state in states {
                if ends(state) || (count == slots && pattern.positional_rest) {
                    alive[count].insert(state);
                    accepted.push(state);
                }
            }
        }
        for step in (0..slots).rev() {
            let live: Vec<usize> = (forward[step].iter())
                .copied()
                .filter(|&state| {
                    (steps(state).iter()).any(|(_, next)| alive[step + 1].contains(next))
                })
                .collect();
            alive[step].extend(live);
        }
        if !alive[0].contains(&0) {
            return None;
        }
        let bottom = self.db.bottom();
        let types = (0..slots)
            .map(|step| {
                let mut ty = bottom;
                for &state in &alive[step] {
                    for (index, next) in steps(state) {
                        if alive[step + 1].contains(&next) {
                            ty = self.lub(ty, atoms[index].1);
                        }
                    }
                }
                ty
            })
            .collect();
        // The tail is the longest suffix left, whose atoms before the shortest
        // may take no item
        let first = accepted.iter().copied().min().expect("an accepted state");
        let last = accepted.iter().copied().max().expect("an accepted state");
        let tail = (first..atoms.len())
            .map(|index| {
                let (mut multiplicity, ty) = atoms[index];
                if index < last && multiplicity == Multiplicity::Required {
                    multiplicity = Multiplicity::Optional;
                }
                SchemaItem {
                    multiplicity,
                    element: Element::Positional(ty),
                }
            })
            .collect();
        Some((types, tail))
    }

    /// The keyed slots' types and the keyed tail. A literal key takes the items
    /// of the literal-keyed atoms owning it; a domain keeps its items, since the
    /// slots can't take every key in it.
    fn walk_keyed(
        &self,
        atoms: &[(Multiplicity, TypeId, TypeId)],
        pattern: &PatternShape,
    ) -> Result<Option<Lane>, Issue> {
        let unknown = self.db.unknown();
        let keys: Vec<TypeId> = atoms.iter().map(|&(_, key, _)| key).collect();
        let literal = |key: TypeId| self.db.literal(key).is_some();
        let mut taken = vec![false; atoms.len()];
        let mut types = Vec::new();
        for &(key, defaulted) in &pattern.keyed {
            let owning = self.owning(key, &keys)?;
            if owning.is_empty() && !defaulted {
                return Ok(None);
            }
            let mut ty = self.db.bottom();
            for (index, _) in owning {
                let (_, owner, value) = atoms[index];
                ty = self.lub(ty, value);
                if owner != unknown && literal(key) && literal(owner) {
                    taken[index] = true;
                }
            }
            types.push(ty);
        }
        let left = atoms.iter().zip(&taken).filter(|&(_, &taken)| !taken);
        let mut tail = Vec::new();
        for (&(multiplicity, key, value), _) in left {
            if multiplicity == Multiplicity::Required && !pattern.keyed_rest {
                return Ok(None);
            }
            tail.push(SchemaItem {
                multiplicity,
                element: Element::Keyed { key, value },
            });
        }
        Ok(Some((types, tail)))
    }

    /// A tail admitting each of several. Positional items become one repeated
    /// item of their join, unless the tails' positional items are all the same.
    /// A literal key is required where every tail requires it; domains repeat.
    fn join_tails<'t>(&self, tails: impl Iterator<Item = &'t Tail> + Clone) -> Tail {
        let mut joined = Tail::default();
        let first = tails.clone().next().expect("a tail");
        if tails
            .clone()
            .all(|tail| tail.positional == first.positional)
        {
            joined.positional = first.positional.clone();
        } else {
            let mut ty = self.db.bottom();
            for item in tails.clone().flat_map(|tail| &tail.positional) {
                if let Element::Positional(item) = item.element {
                    ty = self.lub(ty, item);
                }
            }
            if ty != self.db.bottom() {
                joined.positional.push(SchemaItem {
                    multiplicity: Multiplicity::Repeated,
                    element: Element::Positional(ty),
                });
            }
        }
        let count = tails.clone().count();
        // Each key, how many tails require it, and the join of its values
        let mut keys: Vec<(TypeId, Multiplicity, usize, TypeId)> = Vec::new();
        for item in tails.flat_map(|tail| &tail.keyed) {
            let Element::Keyed { key, value } = item.element else {
                continue;
            };
            let required = usize::from(item.multiplicity == Multiplicity::Required);
            match keys.iter_mut().find(|entry| entry.0 == key) {
                Some(entry) => {
                    if item.multiplicity == Multiplicity::Repeated {
                        entry.1 = Multiplicity::Repeated;
                    }
                    entry.2 += required;
                    entry.3 = self.lub(entry.3, value);
                }
                None => keys.push((key, item.multiplicity, required, value)),
            }
        }
        for (key, multiplicity, required, value) in keys {
            let multiplicity = match multiplicity {
                Multiplicity::Repeated => Multiplicity::Repeated,
                _ if self.db.literal(key).is_none() => Multiplicity::Repeated,
                _ if required == count => Multiplicity::Required,
                _ => Multiplicity::Optional,
            };
            joined.keyed.push(SchemaItem {
                multiplicity,
                element: Element::Keyed { key, value },
            });
        }
        joined
    }
}
