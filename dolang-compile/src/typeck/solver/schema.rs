//! Schema inclusion.
//!
//! A schema admits item sequences. Positional items are distributed by count,
//! as the runtime binds positional arguments: each required item takes one,
//! optional items take what is left over from left to right, and a repeated
//! item takes the rest. Keyed items are unordered, and a literal key owns every
//! item with that key, as a named parameter does. Positional and keyed items are
//! independent.
//!
//! Both sides are flattened into lanes of atoms, splicing inclusions. Schemas
//! that can't be exposed stay opaque: the same rigid on both sides pairs up, an
//! actual rigid otherwise stands for its bound, and the dynamic schema leaves
//! the lanes it occupies unchecked. Inclusion holds when every way of filling
//! the actual side's multiplicities fits the expected side; each actual atom
//! must then fit every expected atom its items can land on.
//!
//! Where the expected side has several keyed domains, as a lookup's
//! `{*(K): V, ...}` does, an item belongs to the narrowest domain that admits
//! its key, as a literal key owns its items, rather than to any domain that
//! would take it: otherwise a domain of every key would leave the others
//! nothing to check.

use std::collections::BTreeSet;

use super::*;

/// How many ways of filling the actual side's multiplicities are tried before
/// an alignment is given up
const COMBINATIONS: usize = 4096;

#[derive(Clone, Copy, Debug)]
struct Atom {
    multiplicity: Multiplicity,
    ty: Term,
    /// The top-level item it came from, for diagnostics
    item: usize,
}

#[derive(Clone, Copy, Debug)]
struct KeyedAtom {
    multiplicity: Multiplicity,
    key: Term,
    value: Term,
    item: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opacity {
    Unknown,
    /// A rigid, as its closed view, or a skolem
    Rigid(Term),
    Infer(InferVarId),
}

#[derive(Clone, Copy, Debug)]
struct Opaque {
    opacity: Opacity,
    /// Whether it admits positional items, keyed items, or both
    lanes: Rest,
    item: usize,
}

impl Opaque {
    fn positional(&self) -> bool {
        self.lanes != Rest::Keyed
    }

    fn keyed(&self) -> bool {
        self.lanes != Rest::Positional
    }
}

/// What an expected schema variable takes from the actual side
#[derive(Clone, Copy, Debug)]
enum Collected {
    Atom(Atom),
    Keyed(KeyedAtom),
    /// The dynamic schema, from the item given
    Unknown(usize),
}

#[derive(Clone, Copy, Debug)]
enum Slot {
    Atom(Atom),
    /// An index into [`Shape::opaque`]. Every opaque has a slot here, whatever
    /// its lanes, so pairs keep their positional order.
    Opaque(usize),
}

#[derive(Default, Debug)]
struct Shape {
    positional: Vec<Slot>,
    keyed: Vec<KeyedAtom>,
    opaque: Vec<Opaque>,
}

/// The inclusive range of a multiplicity's counts; `None` is unbounded
fn range(multiplicity: Multiplicity) -> (usize, Option<usize>) {
    match multiplicity {
        Multiplicity::Required => (1, Some(1)),
        Multiplicity::Optional => (0, Some(1)),
        Multiplicity::Repeated => (0, None),
    }
}

impl Solver<'_> {
    /// Relate two exposed schemas.
    pub(super) fn schemas(
        &self,
        av: TypeView,
        xs: &[SchemaItem],
        bv: TypeView,
        ys: &[SchemaItem],
        expected: Term,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        if let Some(shape) = rest_shape(ys) {
            return self.rest_shaped(av, xs, bv, shape, expected, obligation);
        }
        let mut b = Shape::default();
        self.flatten(bv, ys, None, None, &mut b, 0)?;
        let mut a = Shape::default();
        self.flatten(av, xs, None, None, &mut a, 0)?;
        let pairs = self.pair(&a, &b)?;
        // Every actual rigid without a counterpart stands for its bound
        let paired: HashSet<Term> = pairs
            .iter()
            .filter_map(|&(i, _)| match a.opaque[i].opacity {
                Opacity::Rigid(ty) => Some(ty),
                _ => None,
            })
            .collect();
        let paired_variables: HashSet<InferVarId> = pairs
            .iter()
            .filter_map(|&(i, _)| match a.opaque[i].opacity {
                Opacity::Infer(id) => Some(id),
                _ => None,
            })
            .collect();
        if a.opaque
            .iter()
            .any(|o| matches!(o.opacity, Opacity::Rigid(ty) if !paired.contains(&ty)))
        {
            a = Shape::default();
            self.flatten(av, xs, None, Some(&paired), &mut a, 0)?;
        }
        // Nothing but itself is known to be admitted by an expected rigid
        let b_paired: HashSet<usize> = pairs.iter().map(|&(_, j)| j).collect();
        if b.opaque
            .iter()
            .enumerate()
            .any(|(j, o)| matches!(o.opacity, Opacity::Rigid(_)) && !b_paired.contains(&j))
        {
            return Err(Issue::Contradiction(Contradiction::Rigid));
        }
        // A variable without a counterpart takes what the other side has left. On
        // the actual side, it is bounded only through a rest-shaped expected schema.
        if a.opaque
            .iter()
            .any(|o| matches!(o.opacity, Opacity::Infer(id) if !paired_variables.contains(&id)))
        {
            return Err(Residual::Inference.into());
        }
        let variables: Vec<usize> = (0..b.opaque.len())
            .filter(|j| matches!(b.opaque[*j].opacity, Opacity::Infer(_)) && !b_paired.contains(j))
            .collect();
        match variables[..] {
            [] => {}
            [variable] => return self.collect(&a, &b, variable, obligation),
            _ => return Err(Residual::Inference.into()),
        }
        let open = |shape: &Shape, lane: fn(&Opaque) -> bool| {
            shape
                .opaque
                .iter()
                .any(|o| o.opacity == Opacity::Unknown && lane(o))
        };
        if !open(&a, Opaque::positional) && !open(&b, Opaque::positional) {
            self.positional(&a, &b, obligation)?;
        }
        self.keyed(
            &a.keyed,
            &b.keyed,
            open(&a, Opaque::keyed),
            open(&b, Opaque::keyed),
            None,
            obligation,
        )
    }

    /// Relate schemas where the expected side has one variable without a
    /// counterpart. It must end its positional lane, after required items only,
    /// and it takes the keyed items that the expected side doesn't name. What it
    /// takes becomes its lower bound.
    fn collect(
        &self,
        a: &Shape,
        b: &Shape,
        variable: usize,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        let var = b.opaque[variable];
        let Opacity::Infer(id) = var.opacity else {
            unreachable!()
        };
        let unknown = |shape: &Shape, lane: fn(&Opaque) -> bool| {
            shape
                .opaque
                .iter()
                .any(|o| o.opacity == Opacity::Unknown && lane(o))
        };
        if unknown(b, Opaque::positional) || unknown(b, Opaque::keyed) {
            return Err(Residual::Inference.into());
        }
        let mut collected = Vec::new();
        if var.positional() {
            let mut fixed = Vec::new();
            let mut last = false;
            for slot in &b.positional {
                match *slot {
                    Slot::Atom(atom) if !last && atom.multiplicity == Multiplicity::Required => {
                        fixed.push(atom);
                    }
                    Slot::Opaque(index) if index == variable => last = true,
                    Slot::Opaque(index) if !b.opaque[index].positional() => {}
                    _ => return Err(Residual::Inference.into()),
                }
            }
            let mut atoms = Vec::new();
            for slot in &a.positional {
                match *slot {
                    Slot::Atom(atom) => atoms.push(Collected::Atom(atom)),
                    Slot::Opaque(index) if a.opaque[index].opacity == Opacity::Unknown => {
                        atoms.push(Collected::Unknown(a.opaque[index].item));
                    }
                    Slot::Opaque(index) if !a.opaque[index].positional() => {}
                    Slot::Opaque(_) => return Err(Residual::Inference.into()),
                }
            }
            for (index, y) in fixed.iter().enumerate() {
                match atoms.get(index) {
                    Some(Collected::Atom(x)) if x.multiplicity == Multiplicity::Required => {
                        self.derive(obligation, x.ty, y.ty, Step::Item(x.item));
                    }
                    None if atoms.iter().all(|x| {
                        matches!(x, Collected::Atom(x) if x.multiplicity == Multiplicity::Required)
                    }) =>
                    {
                        return Err(Issue::Contradiction(Contradiction::Missing(y.item)));
                    }
                    _ => return Err(Residual::Alignment.into()),
                }
            }
            collected.extend(atoms.into_iter().skip(fixed.len()));
        } else if !unknown(a, Opaque::positional) {
            self.positional(a, b, obligation)?;
        }
        if var.keyed() {
            // A domain could take the same items
            for y in &b.keyed {
                if !self.literal(y.key)? {
                    return Err(Residual::Inference.into());
                }
            }
            let mut overflow = Vec::new();
            self.keyed(
                &a.keyed,
                &b.keyed,
                unknown(a, Opaque::keyed),
                false,
                Some(&mut overflow),
                obligation,
            )?;
            collected.extend(overflow.into_iter().map(Collected::Keyed));
            if unknown(a, Opaque::keyed) && !var.positional() {
                collected.push(Collected::Unknown(var.item));
            }
        } else {
            self.keyed(
                &a.keyed,
                &b.keyed,
                unknown(a, Opaque::keyed),
                false,
                None,
                obligation,
            )?;
        }
        let taken = self.synthetic(&collected);
        self.derive(obligation, taken, Term::Infer(id), Step::Item(var.item));
        Ok(())
    }

    /// A schema term of collected items, built around their solver terms since
    /// canonical schemas can't hold them
    fn synthetic(&self, collected: &[Collected]) -> Term {
        let mut group = Vec::new();
        let mut slot = |term: Term, kind| {
            group.push(term);
            self.db.intern(Type::Bound {
                reference: BoundRef::new(0, group.len() - 1),
                kind,
            })
        };
        let items: Vec<_> = collected
            .iter()
            .map(|collected| match *collected {
                Collected::Atom(atom) => SchemaItem {
                    multiplicity: atom.multiplicity,
                    element: Element::Positional(slot(atom.ty, Kind::Type)),
                },
                Collected::Keyed(atom) => SchemaItem {
                    multiplicity: atom.multiplicity,
                    element: Element::Keyed {
                        key: slot(atom.key, Kind::Type),
                        value: slot(atom.value, Kind::Type),
                    },
                },
                Collected::Unknown(_) => SchemaItem {
                    multiplicity: Multiplicity::Required,
                    element: Element::Include(self.db.unknown_schema()),
                },
            })
            .collect();
        let schema = self.db.intern(Type::Schema(items.into()));
        let environment = self.intern_environment(self.empty_environment(), group);
        self.view(schema, environment)
    }

    /// Include a schema in one whose items are all repeated: at most one
    /// positional and one keyed. Each of `xs`'s items must fit the matching
    /// repeated item, whatever its multiplicity, and each inclusion must fit
    /// the whole expected schema.
    fn rest_shaped(
        &self,
        av: TypeView,
        xs: &[SchemaItem],
        bv: TypeView,
        RestShape { positional, keyed }: RestShape,
        expected: Term,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        for (index, item) in xs.iter().enumerate() {
            let admitted = match item.element {
                Element::Positional(_) => positional.is_some(),
                Element::Keyed { .. } => keyed.is_some(),
                Element::Include(_) => true,
            };
            if !admitted {
                if matches!(item.element, Element::Positional(_))
                    && let Some((key, _)) = keyed
                    && self.int_keyed(bv.child(key))?
                {
                    return Err(
                        Residual::Unsupported("a position that may be an Int-keyed item").into(),
                    );
                }
                return Err(Issue::Contradiction(Contradiction::Excess(index)));
            }
        }
        for (index, item) in xs.iter().enumerate() {
            match (&item.element, positional, keyed) {
                (&Element::Positional(ty), Some(p), _) => {
                    self.derive(obligation, av.child(ty), bv.child(p), Step::Item(index));
                }
                (&Element::Keyed { key, value }, _, Some((k, v))) => {
                    self.derive(obligation, av.child(key), bv.child(k), Step::Key(index));
                    self.derive(obligation, av.child(value), bv.child(v), Step::Item(index));
                }
                (&Element::Include(schema), _, _) => {
                    self.derive(obligation, av.child(schema), expected, Step::Item(index));
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    /// Flatten items into `shape`. `item` is the top-level item they belong to,
    /// if they are nested. With `keep`, a rigid or skolem outside it is replaced
    /// by its bound; otherwise every one is opaque.
    fn flatten(
        &self,
        view: TypeView,
        items: &[SchemaItem],
        item: Option<usize>,
        keep: Option<&HashSet<Term>>,
        shape: &mut Shape,
        depth: usize,
    ) -> Result<(), Issue> {
        self.depth(depth)?;
        for (index, schema_item) in items.iter().enumerate() {
            self.spend()?;
            let index = item.unwrap_or(index);
            let multiplicity = schema_item.multiplicity;
            match schema_item.element {
                Element::Positional(ty) => shape.positional.push(Slot::Atom(Atom {
                    multiplicity,
                    ty: view.child(ty),
                    item: index,
                })),
                Element::Keyed { key, value } => shape.keyed.push(KeyedAtom {
                    multiplicity,
                    key: view.child(key),
                    value: view.child(value),
                    item: index,
                }),
                Element::Include(schema) => {
                    self.include(
                        view.child(schema),
                        multiplicity,
                        index,
                        keep,
                        shape,
                        depth + 1,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Flatten an included schema into `shape`
    fn include(
        &self,
        term: Term,
        multiplicity: Multiplicity,
        item: usize,
        keep: Option<&HashSet<Term>>,
        shape: &mut Shape,
        depth: usize,
    ) -> Result<(), Issue> {
        self.depth(depth)?;
        let view = match self.head(term)? {
            Head::Structural(view) => view,
            Head::Infer(id) if multiplicity == Multiplicity::Required => {
                shape.positional.push(Slot::Opaque(shape.opaque.len()));
                shape.opaque.push(Opaque {
                    opacity: Opacity::Infer(id),
                    lanes: self.inference[id.0].lanes,
                    item,
                });
                return Ok(());
            }
            Head::Infer(_) => return Err(Residual::Inference.into()),
            Head::Skolem(id) => {
                let Some(bound) = self.skolems[id.0].bound.get() else {
                    return Err(Residual::Unsupported("an included skolem without a bound").into());
                };
                return self.include_rigid(term, bound, multiplicity, item, keep, shape, depth);
            }
            Head::Nominal(_) => {
                return Err(Residual::Unsupported("a class included in a schema").into());
            }
        };
        let opaque = |shape: &mut Shape, opacity, lanes| {
            shape.positional.push(Slot::Opaque(shape.opaque.len()));
            shape.opaque.push(Opaque {
                opacity,
                lanes,
                item,
            });
        };
        match self.db.ty(view.ty) {
            Type::Unknown(_) => {
                opaque(shape, Opacity::Unknown, Rest::All);
                Ok(())
            }
            Type::Schema(items) if multiplicity == Multiplicity::Required => {
                self.flatten(view, items, Some(item), keep, shape, depth + 1)
            }
            // A single item takes on the multiplicity; several would correlate
            // their counts, which lanes can't express
            Type::Schema(items) => {
                let mut inner = Shape::default();
                self.flatten(view, items, Some(item), keep, &mut inner, depth + 1)?;
                match (&inner.positional[..], &inner.keyed[..]) {
                    ([Slot::Atom(atom)], []) => shape.positional.push(Slot::Atom(Atom {
                        multiplicity: multiplicity.compose(atom.multiplicity),
                        ..*atom
                    })),
                    ([], [atom]) => shape.keyed.push(KeyedAtom {
                        multiplicity: multiplicity.compose(atom.multiplicity),
                        ..*atom
                    }),
                    _ => {
                        return Err(
                            Residual::Unsupported("a repeated inclusion of several items").into(),
                        );
                    }
                }
                Ok(())
            }
            Type::Rigid { .. } => {
                self.rigid(view.ty)?;
                let Some(bound) = self.rigid_bound(view.ty) else {
                    return Err(Residual::Unsupported("an included rigid without a bound").into());
                };
                let rigid = self.closed(view.ty);
                let bound = self.closed(bound);
                self.include_rigid(rigid, bound, multiplicity, item, keep, shape, depth)
            }
            _ => Err(Residual::Unsupported("this kind of included schema").into()),
        }
    }

    /// Flatten an included rigid or skolem into `shape`: opaque, or its bound if
    /// `keep` leaves it out
    #[expect(clippy::too_many_arguments, reason = "an inclusion's parts")]
    fn include_rigid(
        &self,
        rigid: Term,
        bound: Term,
        multiplicity: Multiplicity,
        item: usize,
        keep: Option<&HashSet<Term>>,
        shape: &mut Shape,
        depth: usize,
    ) -> Result<(), Issue> {
        if keep.is_some_and(|keep| !keep.contains(&rigid)) {
            return self.include(bound, multiplicity, item, keep, shape, depth + 1);
        }
        if multiplicity != Multiplicity::Required {
            return Err(Residual::Unsupported("an optional or repeated included rigid").into());
        }
        let lanes = self.lanes(bound, depth + 1)?;
        shape.positional.push(Slot::Opaque(shape.opaque.len()));
        shape.opaque.push(Opaque {
            opacity: Opacity::Rigid(rigid),
            lanes,
            item,
        });
        Ok(())
    }

    /// Which lanes the schemas below a bound can occupy
    fn lanes(&self, bound: Term, depth: usize) -> Result<Rest, Issue> {
        let mut shape = Shape::default();
        self.include(bound, Multiplicity::Required, 0, None, &mut shape, depth)?;
        let mut positional = shape
            .positional
            .iter()
            .any(|slot| matches!(slot, Slot::Atom(_)));
        let mut keyed = !shape.keyed.is_empty();
        for opaque in &shape.opaque {
            positional |= opaque.positional();
            keyed |= opaque.keyed();
        }
        Ok(match (positional, keyed) {
            (true, false) => Rest::Positional,
            (false, true) => Rest::Keyed,
            _ => Rest::All,
        })
    }

    /// Pair the same rigid or variable on both sides, in order. Returns pairs of opaque
    /// indices, actual first.
    fn pair(&self, a: &Shape, b: &Shape) -> Result<Vec<(usize, usize)>, Issue> {
        let mut pairs = Vec::new();
        let mut used = HashSet::new();
        for (i, x) in a.opaque.iter().enumerate() {
            if x.opacity == Opacity::Unknown {
                continue;
            }
            if let Some(j) =
                (0..b.opaque.len()).find(|j| !used.contains(j) && b.opaque[*j].opacity == x.opacity)
            {
                used.insert(j);
                pairs.push((i, j));
            }
        }
        if !pairs.is_sorted_by_key(|&(_, j)| j) {
            return Err(Residual::Alignment.into());
        }
        Ok(pairs)
    }

    /// Relate the positional lanes, segment by segment between paired opaques
    fn positional(&self, a: &Shape, b: &Shape, obligation: ObligationId) -> Result<(), Issue> {
        let segments = |shape: &Shape| {
            let mut segments = vec![Vec::new()];
            for slot in &shape.positional {
                match *slot {
                    Slot::Atom(atom) => segments.last_mut().unwrap().push(atom),
                    Slot::Opaque(index) if shape.opaque[index].positional() => {
                        segments.push(Vec::new());
                    }
                    Slot::Opaque(_) => {}
                }
            }
            segments
        };
        let xs = segments(a);
        let ys = segments(b);
        assert_eq!(xs.len(), ys.len(), "positional opaques pair up");
        if let ([x], [y]) = (&xs[..], &ys[..])
            && !x.is_empty()
            && y.is_empty()
        {
            for domain in &b.keyed {
                if !self.literal(domain.key)? && self.int_keyed(domain.key)? {
                    return Err(
                        Residual::Unsupported("positions that may be Int-keyed items").into(),
                    );
                }
            }
        }
        let last = ys.len() - 1;
        for (index, (x, y)) in xs.iter().zip(&ys).enumerate() {
            // Counts are distributed over the whole lane, so only the final
            // segment may vary in length
            if index != last
                && y.iter()
                    .any(|atom| atom.multiplicity != Multiplicity::Required)
            {
                return Err(Residual::Alignment.into());
            }
            self.align(x, y, obligation)?;
        }
        Ok(())
    }

    /// Align positional atoms by count, trying every way of filling the actual
    /// atoms' multiplicities, and derive each atom's type below every expected
    /// atom its items can land on.
    fn align(&self, xs: &[Atom], ys: &[Atom], obligation: ObligationId) -> Result<(), Issue> {
        let required = ys
            .iter()
            .filter(|y| y.multiplicity == Multiplicity::Required)
            .count();
        let optional = ys
            .iter()
            .filter(|y| y.multiplicity == Multiplicity::Optional)
            .count();
        let repeated = ys
            .iter()
            .filter(|y| y.multiplicity == Multiplicity::Repeated)
            .count();
        // Enough to fill every expected atom and overflow once more
        let cap = required + optional + 2;
        let choices: Vec<usize> = xs
            .iter()
            .map(|x| match x.multiplicity {
                Multiplicity::Required => 1,
                Multiplicity::Optional => 2,
                Multiplicity::Repeated => cap + 1,
            })
            .collect();
        let combinations = choices.iter().try_fold(1usize, |n, &c| {
            n.checked_mul(c).filter(|&n| n <= COMBINATIONS)
        });
        if combinations.is_none() {
            return Err(Residual::Alignment.into());
        }
        let mut pairs = BTreeSet::new();
        let mut digits = vec![0; xs.len()];
        loop {
            self.spend()?;
            let counts: Vec<usize> = xs
                .iter()
                .zip(&digits)
                .map(|(x, &digit)| match x.multiplicity {
                    Multiplicity::Required => 1,
                    _ => digit,
                })
                .collect();
            let n: usize = counts.iter().sum();
            if n < required {
                let missing = ys
                    .iter()
                    .filter(|y| y.multiplicity == Multiplicity::Required)
                    .nth(n)
                    .unwrap();
                return Err(Issue::Contradiction(Contradiction::Missing(missing.item)));
            }
            let filled = optional.min(n - required);
            let extra = n - required - filled;
            if extra > 0 && repeated == 0 {
                let (excess, _) = owner(&counts, required + optional);
                return Err(Issue::Contradiction(Contradiction::Excess(xs[excess].item)));
            }
            if extra > 0 && repeated > 1 {
                return Err(Residual::Alignment.into());
            }
            let mut position = 0;
            let mut optionals = 0;
            for (j, y) in ys.iter().enumerate() {
                let take = match y.multiplicity {
                    Multiplicity::Required => 1,
                    Multiplicity::Optional => {
                        optionals += 1;
                        usize::from(optionals <= filled)
                    }
                    Multiplicity::Repeated => extra,
                };
                for p in position..position + take {
                    pairs.insert((owner(&counts, p).0, j));
                }
                position += take;
            }
            // Advance to the next combination
            let mut index = 0;
            loop {
                if index == digits.len() {
                    for (i, j) in pairs {
                        self.derive(obligation, xs[i].ty, ys[j].ty, Step::Item(xs[i].item));
                    }
                    return Ok(());
                }
                digits[index] += 1;
                if digits[index] < choices[index] {
                    break;
                }
                digits[index] = 0;
                index += 1;
            }
        }
    }

    /// Relate keyed items. `open_actual` and `open_expected` say that the dynamic
    /// schema occupies that side's keyed lane: the actual side may then have
    /// more keys than it shows, and the expected side admits anything. With
    /// `overflow`, the items that no literal key claims go there instead of to a
    /// domain, as do domain items, after counting toward the literal keys they
    /// may hold.
    fn keyed(
        &self,
        xs: &[KeyedAtom],
        ys: &[KeyedAtom],
        open_actual: bool,
        open_expected: bool,
        mut overflow: Option<&mut Vec<KeyedAtom>>,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        let (literals, domains): (Vec<_>, Vec<_>) = ys
            .iter()
            .map(|y| Ok((self.literal(y.key)?, y)))
            .collect::<Result<Vec<_>, Issue>>()?
            .into_iter()
            .partition(|(literal, _)| *literal);
        let literals: Vec<&KeyedAtom> = literals.into_iter().map(|(_, y)| y).collect();
        let domains: Vec<&KeyedAtom> = domains.into_iter().map(|(_, y)| y).collect();
        // Several domains own items by their keys, so each key must be known
        if domains.len() > 1 {
            for domain in &domains {
                if let Head::Infer(_) = self.head(domain.key)? {
                    return Err(Residual::Inference.into());
                }
            }
        }
        for (i, y) in literals.iter().enumerate() {
            for other in &literals[..i] {
                if self.same(y.key, other.key)? {
                    return Err(Residual::Unsupported("two literal keys that are the same").into());
                }
            }
        }
        let mut claimed = vec![false; xs.len()];
        for y in &literals {
            let (mut low, mut high) = (0, Some(0));
            let mut last = None;
            for (i, x) in xs.iter().enumerate() {
                let contributes = if self.literal(x.key)? {
                    self.same(x.key, y.key)?
                } else {
                    self.admits(x.key, y.key)?
                };
                if !contributes {
                    continue;
                }
                let (lo, hi) = if self.literal(x.key)? {
                    claimed[i] = true;
                    range(x.multiplicity)
                } else {
                    (0, None)
                };
                low += lo;
                high = high.zip(hi).map(|(a, b)| a + b);
                last = Some(x.item);
                self.derive(obligation, x.value, y.value, Step::Item(x.item));
            }
            if open_expected {
                continue;
            }
            let (min, max) = range(y.multiplicity);
            if low < min && !open_actual {
                return Err(Issue::Contradiction(Contradiction::Missing(y.item)));
            }
            if max.is_some_and(|max| high.is_none_or(|high| high > max)) {
                return Err(Issue::Contradiction(Contradiction::Excess(last.unwrap())));
            }
        }
        for (i, x) in xs.iter().enumerate() {
            if claimed[i] {
                continue;
            }
            if let Some(overflow) = overflow.as_deref_mut() {
                overflow.push(*x);
                continue;
            }
            match domains[..] {
                [] if open_expected => {}
                [] if self.literal(x.key)? || self.nominal_head(x.key)? => {
                    return Err(Issue::Contradiction(Contradiction::Excess(x.item)));
                }
                [domain] if domain.multiplicity == Multiplicity::Repeated => {
                    self.derive(obligation, x.key, domain.key, Step::Key(x.item));
                    self.derive(obligation, x.value, domain.value, Step::Item(x.item));
                }
                [_, _, ..]
                    if (domains.iter()).all(|d| d.multiplicity == Multiplicity::Repeated) =>
                {
                    self.owners(x, &domains, open_expected, obligation)?;
                }
                _ => {
                    return Err(Residual::Unsupported(
                        "several key domains that aren't all repeated",
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    /// Give an actual keyed atom to the expected domains that own its keys. Each
    /// member of its key goes to the narrowest domain that admits it, and to any
    /// domain lying inside it, which owns part of it; its value must fit each.
    fn owners(
        &self,
        x: &KeyedAtom,
        domains: &[&KeyedAtom],
        open_expected: bool,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        let keys: Vec<TypeId> = (domains.iter())
            .map(|domain| self.reify(domain.key))
            .collect::<Result<_, _>>()?;
        for member in self.union_members(self.reify(x.key)?) {
            let UnionMember::Type(member) = member else {
                return Err(Residual::Unsupported("a key with projections").into());
            };
            let owning = self.owning(member, &keys)?;
            if owning.is_empty() {
                let view = self.closed(member);
                if open_expected {
                    continue;
                }
                if self.literal(view)? || self.nominal_head(view)? {
                    return Err(Issue::Contradiction(Contradiction::Excess(x.item)));
                }
                return Err(
                    Residual::Unsupported("a key no domain of a closed schema owns").into(),
                );
            }
            for (d, _) in owning {
                let item = Step::Item(x.item);
                self.derive(obligation, x.value, domains[d].value, item);
            }
        }
        Ok(())
    }

    /// The domains among `keys` that own a key: the narrowest that admits it
    /// whole, and any lying inside it, which owns part of it. Each is given with
    /// whether it admits the key whole. A domain lies inside a key only if the key
    /// isn't a literal. None own a key no domain overlaps.
    pub(super) fn owning(&self, key: TypeId, keys: &[TypeId]) -> Result<Vec<(usize, bool)>, Issue> {
        let literal = self.db.literal(key).is_some();
        let mut overlapping = Vec::new();
        for (d, &domain) in keys.iter().enumerate() {
            let whole = self.probe(key, domain)?;
            if whole == Status::Proven {
                overlapping.push((d, true));
                continue;
            }
            let inside = match literal {
                true => Status::Contradicted,
                false => self.probe(domain, key)?,
            };
            match (whole, inside) {
                (_, Status::Proven) => overlapping.push((d, false)),
                (Status::Contradicted, Status::Contradicted) => {}
                _ => {
                    return Err(Residual::Unsupported(
                        "a key that may or may not overlap a domain",
                    )
                    .into());
                }
            }
        }
        let mut owning = Vec::new();
        for &(d, whole) in &overlapping {
            // A strictly narrower domain admitting the whole key owns it instead.
            // Domains each below the other, as the dynamic type is below any, both
            // own it.
            let mut narrowed = false;
            for &(e, whole) in &overlapping {
                narrowed |= whole
                    && keys[e] != keys[d]
                    && self.probe(keys[e], keys[d])? == Status::Proven
                    && self.probe(keys[d], keys[e])? != Status::Proven;
            }
            if !narrowed {
                owning.push((d, whole));
            }
        }
        Ok(owning)
    }

    /// Whether a key is a single literal
    fn literal(&self, key: Term) -> Result<bool, Issue> {
        Ok(matches!(
            self.head(key)?,
            Head::Structural(view) if matches!(self.db.ty(view.ty), Type::Literal(_))
        ))
    }

    /// Whether a key domain might admit `Int`. A positional item then might be one
    /// of its keyed items, by a one-way rule that is not supported yet. Without
    /// `Int`, positions are not keys.
    fn int_keyed(&self, key: Term) -> Result<bool, Issue> {
        let Some(int) = self.db.intrinsic(Intrinsic::Int) else {
            return Ok(false);
        };
        Ok(self.probe(int, self.reify(key)?)? != Status::Contradicted)
    }

    fn nominal_head(&self, term: Term) -> Result<bool, Issue> {
        Ok(matches!(self.head(term)?, Head::Nominal(_)))
    }

    /// Whether a key domain admits a literal key, decided without adding bounds
    fn admits(&self, domain: Term, literal: Term) -> Result<bool, Issue> {
        match self.probe(self.reify(literal)?, self.reify(domain)?)? {
            Status::Proven => Ok(true),
            Status::Contradicted => Ok(false),
            Status::Unresolved => {
                Err(Residual::Unsupported("a literal key a domain may or may not admit").into())
            }
        }
    }
}

/// The atom whose items include position `p`, and the position within it
fn owner(counts: &[usize], mut p: usize) -> (usize, usize) {
    for (i, &count) in counts.iter().enumerate() {
        if p < count {
            return (i, p);
        }
        p -= count;
    }
    unreachable!("position past the actual items")
}

/// The element types of a schema whose items are all repeated, at most one
/// positional and one keyed
struct RestShape {
    positional: Option<TypeId>,
    /// The key and value types
    keyed: Option<(TypeId, TypeId)>,
}

fn rest_shape(ys: &[SchemaItem]) -> Option<RestShape> {
    let (mut positional, mut keyed) = (None, None);
    for item in ys {
        match (item.multiplicity, &item.element) {
            (Multiplicity::Repeated, &Element::Positional(ty)) if positional.is_none() => {
                positional = Some(ty);
            }
            (Multiplicity::Repeated, &Element::Keyed { key, value }) if keyed.is_none() => {
                keyed = Some((key, value));
            }
            _ => return None,
        }
    }
    Some(RestShape { positional, keyed })
}
