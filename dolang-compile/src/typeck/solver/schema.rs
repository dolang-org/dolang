//! Schema inclusion.
//!
//! A schema admits item sequences. Positional items are distributed by count,
//! as the runtime binds positional arguments: each required item takes one,
//! optional items take what is left over from left to right, and a repeated
//! item takes the rest. Keyed items are unordered across keys; within a key, a
//! literal key takes as many items as it admits, in order, as a named parameter
//! does, and a domain takes the rest. Positional and keyed items are
//! independent.
//!
//! A parameter list binds by count, so on the actual side it fills only as
//! counting does. Other schemas may fill their multiplicities any way that
//! fits, and the schema a value holds may also be taken by any filling of the
//! expected side: where counting is refuted, a filling still fits when its
//! types prove some path ([`Fill`]).
//!
//! Both sides are flattened into lanes of atoms, splicing inclusions. Schemas
//! that can't be exposed stay opaque: the same rigid on both sides pairs up, an
//! actual rigid otherwise stands for its bound, and the dynamic schema leaves
//! the lanes it occupies unchecked but for the required items that lead the
//! positional lane, whose positions it can't change. Inclusion holds when every way of filling
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
pub(super) struct Atom {
    pub(super) multiplicity: Multiplicity,
    pub(super) ty: Term,
    /// The top-level item it came from, for diagnostics
    item: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct KeyedAtom {
    pub(super) multiplicity: Multiplicity,
    pub(super) key: Term,
    pub(super) value: Term,
    item: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Opacity {
    Unknown,
    /// A rigid, as its closed view, or a skolem
    Rigid(Term),
    Infer(InferVarId),
    /// A mapping over a pack being inferred, which relates only to a mapping of
    /// the same pattern, pack by pack
    Mapped(Term),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Opaque {
    pub(super) opacity: Opacity,
    /// Whether it admits positional items, keyed items, or both
    lanes: Rest,
    item: usize,
}

impl Opaque {
    pub(super) fn positional(&self) -> bool {
        self.lanes != Rest::Keyed
    }

    pub(super) fn keyed(&self) -> bool {
        self.lanes != Rest::Positional
    }
}

/// What an expected schema variable takes from the actual side
#[derive(Clone, Copy, Debug)]
enum Collected {
    Atom(Atom),
    Keyed(KeyedAtom),
    /// The dynamic schema
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Slot {
    Atom(Atom),
    /// An index into [`Shape::opaque`]. Every opaque has a slot here, whatever
    /// its lanes, so pairs keep their positional order.
    Opaque(usize),
}

#[derive(Default, Debug)]
pub(super) struct Shape {
    pub(super) positional: Vec<Slot>,
    pub(super) keyed: Vec<KeyedAtom>,
    pub(super) opaque: Vec<Opaque>,
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
    /// Choose a closed shape from schema upper bounds. This is deliberately
    /// limited to required literal items; it does not invent schema intersections.
    pub(super) fn upper_schema(&self, upper: &[TypeId]) -> Result<TypeId, Residual> {
        let shape = upper
            .iter()
            .find_map(|&ty| match self.db.ty(ty) {
                Type::Schema(items)
                    if items.iter().all(|item| {
                        item.multiplicity == Multiplicity::Required
                            && match item.element {
                                Element::Positional(_) => true,
                                Element::Keyed { key, .. } => self.db.literal(key).is_some(),
                                Element::Include(_) => false,
                            }
                    }) =>
                {
                    Some(items)
                }
                _ => None,
            })
            .ok_or(Residual::Inference)?;
        let mut position = 0;
        let mut items = Vec::new();
        for source in shape.iter() {
            let (key, mut value) = match source.element {
                Element::Positional(value) => {
                    let key = self.db.intern(Type::Literal(Literal::Int(position)));
                    position += 1;
                    (key, value)
                }
                Element::Keyed { key, value } => (key, value),
                Element::Include(_) => unreachable!("a closed literal shape"),
            };
            for &bound in upper {
                if matches!(self.db.ty(bound), Type::Unknown(_)) {
                    continue;
                }
                let projected = self
                    .schema_item(bound, key)
                    .map_err(|_| Residual::Inference)?;
                if projected == self.db.unknown() {
                    continue;
                }
                value = if value == self.db.unknown() {
                    projected
                } else {
                    self.meet(value, projected)
                        .map_err(|_| Residual::Inference)?
                };
            }
            let element = match source.element {
                Element::Positional(_) => Element::Positional(value),
                _ => Element::Keyed { key, value },
            };
            items.push(SchemaItem {
                multiplicity: Multiplicity::Required,
                element,
            });
        }
        let candidate = self.db.intern(Type::Schema(items.into()));
        for &bound in upper {
            if self
                .probe(candidate, bound)
                .map_err(|_| Residual::Inference)?
                != Status::Proven
            {
                return Err(Residual::Inference);
            }
        }
        Ok(candidate)
    }

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
        let mut pairs = self.pair(&a, &b)?;
        self.cross(&a, &b, &mut pairs, obligation)?;
        // Mappings paired by pattern relate pack by pack; one without a
        // counterpart would need its pack inferred from the other side's items
        for &(i, j) in &pairs {
            match (a.opaque[i].opacity, b.opaque[j].opacity) {
                (Opacity::Mapped(x), Opacity::Mapped(y) | Opacity::Rigid(y))
                | (Opacity::Rigid(x), Opacity::Mapped(y)) => {
                    self.derive(obligation, x, y, Step::Item(a.opaque[i].item));
                }
                _ => {}
            }
        }
        let mapped = |shape: &Shape, paired: &dyn Fn(usize) -> bool| {
            (shape.opaque.iter().enumerate())
                .any(|(index, o)| matches!(o.opacity, Opacity::Mapped(_)) && !paired(index))
        };
        if mapped(&a, &|i| pairs.iter().any(|&(x, _)| x == i))
            || mapped(&b, &|j| pairs.iter().any(|&(_, y)| y == j))
        {
            return Err(Residual::Unsupported("a mapping over a pack being inferred").into());
        }
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
        } else {
            self.prefix(&a, &b, obligation)?;
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
                        atoms.push(Collected::Unknown);
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
                collected.push(Collected::Unknown);
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
                Collected::Unknown => SchemaItem {
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
    /// Whether `pack`, a positional rigid or skolem pack, is below `expected`
    /// because each of its items is: `expected` is a rest-shaped schema whose
    /// repeated positional item is a union expanding the same pack, as
    /// `{...Ts}` is below `{*Union[...Ts]}`
    pub(super) fn expands_into(&self, pack: Term, expected: &Head) -> Result<bool, Issue> {
        let Head::Structural(view) = expected else {
            return Ok(false);
        };
        let Type::Schema(items) = self.db.ty(view.ty) else {
            return Ok(false);
        };
        let Some(RestShape {
            positional: Some(item),
            ..
        }) = rest_shape(items)
        else {
            return Ok(false);
        };
        let item = view.child(item);
        let Head::Structural(union) = self.head(item)? else {
            return Ok(false);
        };
        let Type::Union(members) = self.db.ty(union.ty) else {
            return Ok(false);
        };
        for &member in members.iter() {
            let UnionMember::Expand(mut expanded) = member else {
                continue;
            };
            // `Union[...Ts]` expands `{...Ts}`
            if let Type::Schema(items) = self.db.ty(expanded)
                && let [
                    SchemaItem {
                        multiplicity: Multiplicity::Required,
                        element: Element::Include(included),
                    },
                ] = items[..]
            {
                expanded = included;
            }
            if self.same(pack, union.child(expanded))? {
                return Ok(true);
            }
        }
        Ok(false)
    }

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
                // A mapping's items fit one by one, since it would relate as a
                // schema including it, which is this one
                (&Element::Include(schema), _, _)
                    if let Head::Structural(view) = self.head(av.child(schema))?
                        && let Type::Map { packs, .. } = self.db.ty(view.ty) =>
                {
                    let term = Term::View(view);
                    // Where the expected side takes any value, only the lanes the
                    // mapping occupies and the keys it keeps from its packs
                    // matter, not whether its pattern keeps the order of its
                    // packs' items
                    let any = |term: Term| -> Result<bool, Issue> {
                        Ok(matches!(self.head(term)?, Head::Structural(view)
                            if view.ty == self.db.top() || matches!(self.db.ty(view.ty), Type::Unknown(_))))
                    };
                    let mut lanes = None;
                    for &pack in packs.iter() {
                        let pack = self.lanes(view.child(pack), 1)?;
                        lanes = Some(match lanes {
                            Some(seen) if seen != pack => Rest::All,
                            _ => pack,
                        });
                    }
                    let lanes = lanes.expect("a mapping has a pack");
                    let takes_positional = match (lanes, positional) {
                        (Rest::Keyed, _) => true,
                        (_, Some(p)) => any(bv.child(p))?,
                        (_, None) => false,
                    };
                    let takes_keyed = match (lanes, keyed) {
                        (Rest::Positional, _) => true,
                        (_, Some((_, v))) => any(bv.child(v))?,
                        (_, None) => false,
                    };
                    if takes_positional && takes_keyed {
                        if let (Rest::All | Rest::Keyed, Some((k, _))) = (lanes, keyed) {
                            let top = self.db.top();
                            let keys = self.db.intern(Type::Schema(
                                vec![
                                    SchemaItem {
                                        multiplicity: Multiplicity::Repeated,
                                        element: Element::Positional(top),
                                    },
                                    SchemaItem {
                                        multiplicity: Multiplicity::Repeated,
                                        element: Element::Keyed { key: k, value: top },
                                    },
                                ]
                                .into(),
                            ));
                            for &pack in packs.iter() {
                                let step = Step::Key(index);
                                self.derive(obligation, view.child(pack), bv.child(keys), step);
                            }
                        }
                        continue;
                    }
                    let mut shape = Shape::default();
                    let keep = HashSet::new();
                    self.include(term, item.multiplicity, index, Some(&keep), &mut shape, 0)?;
                    for slot in &shape.positional {
                        match (*slot, positional) {
                            (Slot::Atom(atom), Some(p)) => {
                                self.derive(obligation, atom.ty, bv.child(p), Step::Item(index));
                            }
                            (Slot::Atom(_), None) => {
                                return Err(Issue::Contradiction(Contradiction::Excess(index)));
                            }
                            (Slot::Opaque(o), _) if shape.opaque[o].opacity == Opacity::Unknown => {
                            }
                            (Slot::Opaque(_), _) => {
                                return Err(Residual::Unsupported(
                                    "a mapping over a pack being inferred",
                                )
                                .into());
                            }
                        }
                    }
                    for atom in &shape.keyed {
                        let Some((k, v)) = keyed else {
                            return Err(Issue::Contradiction(Contradiction::Excess(index)));
                        };
                        self.derive(obligation, atom.key, bv.child(k), Step::Key(index));
                        self.derive(obligation, atom.value, bv.child(v), Step::Item(index));
                    }
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
    pub(super) fn include(
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
            Type::Unsupported { .. } => Err(UNREPRESENTED.into()),
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
            Type::Map { packs, pattern } if multiplicity == Multiplicity::Required => {
                self.include_map(view, packs, *pattern, item, keep, shape, depth)
            }
            Type::Map { .. } => {
                Err(Residual::Unsupported("an optional or repeated included mapping").into())
            }
            _ => Err(Residual::Unsupported("this kind of included schema").into()),
        }
    }

    /// Flatten an included mapping into `shape`: its pattern viewed with the
    /// corresponding items of its packs as its group, item by item. Over a
    /// single pack, the dynamic schema stays dynamic, and an opaque rigid or
    /// skolem becomes the mapping over it alone.
    #[expect(clippy::too_many_arguments, reason = "an inclusion's parts")]
    fn include_map(
        &self,
        view: TypeView,
        packs: &[TypeId],
        pattern: TypeId,
        item: usize,
        keep: Option<&HashSet<Term>>,
        shape: &mut Shape,
        depth: usize,
    ) -> Result<(), Issue> {
        let mut flattened = Vec::new();
        for &pack in packs {
            let mut inner = Shape::default();
            self.include(
                view.child(pack),
                Multiplicity::Required,
                item,
                None,
                &mut inner,
                depth + 1,
            )?;
            flattened.push(inner);
        }
        let mapped =
            |items: Vec<Term>| self.view(pattern, self.intern_environment(view.environment, items));
        let inferred = Residual::Unsupported("a mapping over a pack being inferred");
        if let [inner] = &flattened[..] {
            for slot in &inner.positional {
                match *slot {
                    Slot::Atom(atom) => shape.positional.push(Slot::Atom(Atom {
                        ty: mapped(vec![atom.ty]),
                        ..atom
                    })),
                    Slot::Opaque(index) => {
                        let opaque = inner.opaque[index];
                        let opacity = match opaque.opacity {
                            Opacity::Unknown => Opacity::Unknown,
                            Opacity::Infer(id) => {
                                Opacity::Mapped(self.mapping(view, pattern, Term::Infer(id)))
                            }
                            Opacity::Mapped(_) => return Err(inferred.into()),
                            Opacity::Rigid(rigid) => {
                                let mapping = self.mapping(view, pattern, rigid);
                                if keep.is_some_and(|keep| !keep.contains(&mapping)) {
                                    // Standing for the mapping over its bound
                                    // keeps order only if the pattern does
                                    if matches!(
                                        self.db.slot_variance(pattern, 0),
                                        Some(Variance::Invariant | Variance::Contravariant)
                                    ) {
                                        return Err(Residual::Unsupported(
                                            "a mapping over a rigid that its pattern doesn't preserve the order of",
                                        )
                                        .into());
                                    }
                                    let bound = self.opaque_bound(rigid)?;
                                    let mapping = self.mapping(view, pattern, bound);
                                    self.include(
                                        mapping,
                                        Multiplicity::Required,
                                        item,
                                        keep,
                                        shape,
                                        depth + 1,
                                    )?;
                                    continue;
                                }
                                Opacity::Rigid(mapping)
                            }
                        };
                        shape.positional.push(Slot::Opaque(shape.opaque.len()));
                        shape.opaque.push(Opaque { opacity, ..opaque });
                    }
                }
            }
            for atom in &inner.keyed {
                shape.keyed.push(KeyedAtom {
                    value: mapped(vec![atom.value]),
                    ..*atom
                });
            }
            return Ok(());
        }
        // Several packs map item by item only once their items are all known
        if let Some(opaque) = flattened.iter().flat_map(|inner| &inner.opaque).next() {
            return Err(match opaque.opacity {
                Opacity::Infer(_) => inferred,
                _ => Residual::Unsupported("a mapping over several packs not all known"),
            }
            .into());
        }
        let first = &flattened[0];
        if flattened.iter().any(|inner| {
            inner.positional.len() != first.positional.len()
                || inner.keyed.len() != first.keyed.len()
        }) {
            return Err(Issue::Contradiction(Contradiction::MappedPacks));
        }
        for index in 0..first.positional.len() {
            let atoms: Vec<Atom> = (flattened.iter())
                .map(|inner| match inner.positional[index] {
                    Slot::Atom(atom) => atom,
                    Slot::Opaque(_) => unreachable!("no pack has an opaque"),
                })
                .collect();
            if atoms
                .iter()
                .any(|atom| atom.multiplicity != atoms[0].multiplicity)
            {
                return Err(Issue::Contradiction(Contradiction::MappedPacks));
            }
            shape.positional.push(Slot::Atom(Atom {
                ty: mapped(atoms.iter().map(|atom| atom.ty).collect()),
                ..atoms[0]
            }));
        }
        for index in 0..first.keyed.len() {
            let atoms: Vec<KeyedAtom> = flattened.iter().map(|inner| inner.keyed[index]).collect();
            for atom in &atoms[1..] {
                if atom.multiplicity != atoms[0].multiplicity {
                    return Err(Issue::Contradiction(Contradiction::MappedPacks));
                }
                if !self.same(atom.key, atoms[0].key)? {
                    return Err(Residual::Unsupported("mapped packs whose keys may differ").into());
                }
            }
            shape.keyed.push(KeyedAtom {
                value: mapped(atoms.iter().map(|atom| atom.value).collect()),
                ..atoms[0]
            });
        }
        Ok(())
    }

    /// A mapping of `pattern`, viewed in `view`'s environment, over `pack` alone,
    /// closed where it can be so the same mapping on both sides of a judgment is
    /// the same term
    fn mapping(&self, view: TypeView, pattern: TypeId, pack: Term) -> Term {
        let reference = self.db.intern(Type::Bound {
            reference: BoundRef::new(0, 0),
            kind: Kind::Schema,
        });
        // The pack's group is outside the pattern's own
        let pattern = (self.db.shift(pattern, 1, 1)).expect("inserting a group removes none");
        let map = self.db.intern(Type::Map {
            packs: vec![reference].into(),
            pattern,
        });
        let pack = self.resolve(pack).unwrap_or(pack);
        let term = self.view(map, self.intern_environment(view.environment, vec![pack]));
        self.reify(term).map_or(term, |ty| self.closed(ty))
    }

    /// The bound of a rigid or skolem an opaque stands for
    fn opaque_bound(&self, rigid: Term) -> Result<Term, Issue> {
        let bound = match self.head(rigid)? {
            Head::Skolem(id) => self.skolems[id.0].bound.get(),
            Head::Structural(view) if self.rigid(view.ty)?.is_some() => {
                self.rigid_bound(view.ty).map(|bound| self.closed(bound))
            }
            _ => {
                return Err(Residual::Unsupported("a mapping over a mapping over a rigid").into());
            }
        };
        bound.ok_or_else(|| Residual::Unsupported("an included rigid without a bound").into())
    }

    /// Relate mappings of the same pattern pack by pack, at the pattern's
    /// variance in each, when some pack isn't an exposed schema to map item by
    /// item, as a generic signature's own mapping is in its instantiation.
    /// Returns whether they relate so.
    pub(super) fn mappings(
        &self,
        a: TypeView,
        b: TypeView,
        obligation: ObligationId,
    ) -> Result<bool, Issue> {
        if !self.same_pattern(Term::View(a), Term::View(b))? {
            return Ok(false);
        }
        let (Type::Map { packs: xs, pattern }, Type::Map { packs: ys, .. }) =
            (self.db.ty(a.ty), self.db.ty(b.ty))
        else {
            unreachable!("both are mappings")
        };
        let mut exposed = true;
        for pack in (xs.iter().map(|&x| a.child(x))).chain(ys.iter().map(|&y| b.child(y))) {
            exposed &= matches!(self.head(pack)?,
                Head::Structural(view) if matches!(self.db.ty(view.ty), Type::Schema(_)));
        }
        // A pack whose items the pattern doesn't use relates only by its count
        let variances: Option<Vec<_>> = (0..xs.len())
            .map(|index| {
                let index = u16::try_from(index).expect("a mapping's packs fit a group");
                self.db.slot_variance(*pattern, index)
            })
            .collect();
        let (false, Some(variances)) = (exposed, variances) else {
            return Ok(false);
        };
        for (index, variance) in variances.into_iter().enumerate() {
            let (x, y) = (a.child(xs[index]), b.child(ys[index]));
            if variance != Variance::Contravariant {
                let step = Step::Argument {
                    index,
                    reversed: false,
                };
                self.derive(obligation, x, y, step);
            }
            if variance != Variance::Covariant {
                let step = Step::Argument {
                    index,
                    reversed: true,
                };
                self.derive(obligation, y, x, step);
            }
        }
        Ok(true)
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

    /// Pair the same rigid or variable on both sides, in order, and a mapping
    /// over a pack being inferred with one of the same pattern. Returns pairs of
    /// opaque indices, actual first.
    fn pair(&self, a: &Shape, b: &Shape) -> Result<Vec<(usize, usize)>, Issue> {
        let mut pairs = Vec::new();
        let mut used = HashSet::new();
        for (i, x) in a.opaque.iter().enumerate() {
            if x.opacity == Opacity::Unknown {
                continue;
            }
            for (j, y) in b.opaque.iter().enumerate() {
                if used.contains(&j) {
                    continue;
                }
                let paired = match (x.opacity, y.opacity) {
                    (Opacity::Mapped(x), Opacity::Mapped(y) | Opacity::Rigid(y))
                    | (Opacity::Rigid(x), Opacity::Mapped(y)) => self.same_pattern(x, y)?,
                    // A mapping over a skolem has no closed form to compare
                    (Opacity::Rigid(x), Opacity::Rigid(y)) => x == y || self.same(x, y)?,
                    (x, y) => x == y,
                };
                if paired {
                    used.insert(j);
                    pairs.push((i, j));
                    break;
                }
            }
        }
        if !pairs.is_sorted_by_key(|&(_, j)| j) {
            return Err(Residual::Alignment.into());
        }
        Ok(pairs)
    }

    /// Whether two terms are mappings of the same pattern over as many packs
    fn same_pattern(&self, a: Term, b: Term) -> Result<bool, Issue> {
        let (Head::Structural(a), Head::Structural(b)) = (self.head(a)?, self.head(b)?) else {
            return Ok(false);
        };
        let (
            Type::Map { packs: xs, pattern },
            Type::Map {
                packs: ys,
                pattern: other,
            },
        ) = (self.db.ty(a.ty), self.db.ty(b.ty))
        else {
            return Ok(false);
        };
        Ok(
            xs.len() == ys.len()
                && self.same_scoped(a.child(*pattern), 1, b.child(*other), 1, 0)?,
        )
    }

    /// Pair a variable with a rigid or skolem across from it, each without a
    /// counterpart, when both sides have their opaques in the same places, as a
    /// quantified signature related to its own instantiation does. The variable
    /// stands for the rigid, so it is bounded by it.
    fn cross(
        &self,
        a: &Shape,
        b: &Shape,
        pairs: &mut Vec<(usize, usize)>,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        if a.opaque.len() != b.opaque.len() {
            return Ok(());
        }
        let mut crossed = false;
        for (index, (x, y)) in a.opaque.iter().zip(&b.opaque).enumerate() {
            if x.lanes != y.lanes || pairs.iter().any(|&(i, j)| i == index || j == index) {
                continue;
            }
            let (actual, expected) = match (x.opacity, y.opacity) {
                (Opacity::Infer(id), Opacity::Rigid(rigid)) => (Term::Infer(id), rigid),
                (Opacity::Rigid(rigid), Opacity::Infer(id)) => (rigid, Term::Infer(id)),
                _ => continue,
            };
            self.derive(obligation, actual, expected, Step::Item(x.item));
            pairs.push((index, index));
            crossed = true;
        }
        if crossed {
            pairs.sort();
            if !pairs.is_sorted_by_key(|&(_, j)| j) {
                return Err(Residual::Alignment.into());
            }
        }
        Ok(())
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

    /// Relate positional lanes beside the dynamic schema. Each side's leading
    /// required atoms take the first positions whatever it holds, so they pair
    /// up in order, and a closed side must have room for the other's.
    fn prefix(&self, a: &Shape, b: &Shape, obligation: ObligationId) -> Result<(), Issue> {
        let leading = |shape: &Shape| {
            let mut atoms = Vec::new();
            for slot in &shape.positional {
                match *slot {
                    Slot::Atom(atom) if atom.multiplicity == Multiplicity::Required => {
                        atoms.push(atom)
                    }
                    Slot::Opaque(index) if !shape.opaque[index].positional() => {}
                    _ => break,
                }
            }
            atoms
        };
        // The most items a lane without positional opaques holds
        let most = |shape: &Shape| {
            let mut total = 0;
            for slot in &shape.positional {
                match *slot {
                    Slot::Atom(atom) => total += range(atom.multiplicity).1?,
                    Slot::Opaque(index) if !shape.opaque[index].positional() => {}
                    Slot::Opaque(_) => return None,
                }
            }
            Some(total)
        };
        let xs = leading(a);
        let ys = leading(b);
        if let Some(most) = most(a)
            && let Some(missing) = ys.get(most)
        {
            return Err(Issue::Contradiction(Contradiction::Missing(missing.item)));
        }
        if let Some(most) = most(b)
            && let Some(excess) = xs.get(most)
        {
            return Err(Issue::Contradiction(Contradiction::Excess(excess.item)));
        }
        for (x, y) in xs.iter().zip(&ys) {
            self.derive(obligation, x.ty, y.ty, Step::Item(x.item));
        }
        Ok(())
    }

    /// Align positional atoms by count, trying every way of filling the actual
    /// atoms' multiplicities, and derive each atom's type below every expected
    /// atom its items can land on. An actual parameter list fills only as it
    /// binds by count.
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
        // Advance to the next combination, if there is one
        let advance = |digits: &mut [usize]| {
            for (digit, &choice) in digits.iter_mut().zip(&choices) {
                *digit += 1;
                if *digit < choice {
                    return true;
                }
                *digit = 0;
            }
            false
        };
        let fill = match self.obligations[obligation.0].relation {
            Relation::Subtype { fill, .. } => fill,
            Relation::Call(_) => Fill::Arguments,
        };
        let mut decided = HashMap::new();
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
            // A parameter list fills an optional item before any later optional
            // or repeated one
            let unbound = fill == Fill::Parameters
                && xs.iter().zip(&counts).enumerate().any(|(i, (x, &count))| {
                    x.multiplicity == Multiplicity::Optional
                        && count == 0
                        && (xs[i + 1..].iter().zip(&counts[i + 1..])).any(|(later, &count)| {
                            later.multiplicity != Multiplicity::Required && count > 0
                        })
                });
            if unbound {
                if advance(&mut digits) {
                    continue;
                }
                break;
            }
            let counted = self.count(xs, &counts, ys, required, optional, repeated);
            // The items a value holds fit when any filling of the expected side
            // fits them, which counting may miss: it suffices that its types
            // prove a path when counting is refuted
            let refuted = match &counted {
                Err(Issue::Contradiction(_)) => true,
                Err(_) => false,
                Ok(found) => {
                    let mut refuted = false;
                    for &(i, j) in found {
                        refuted |= self.decide(&mut decided, xs, ys, i, j)? == Status::Contradicted;
                    }
                    refuted
                }
            };
            let contents = fill == Fill::Contents;
            if !(contents && refuted && self.filled(&mut decided, xs, &counts, ys)?) {
                pairs.extend(counted?);
            }
            if !advance(&mut digits) {
                break;
            }
        }
        for (i, j) in pairs {
            self.derive(obligation, xs[i].ty, ys[j].ty, Step::Item(xs[i].item));
        }
        Ok(())
    }

    /// The pairs of actual and expected atoms that one filling of the actual
    /// atoms, with `counts` items each, lands on by count
    fn count(
        &self,
        xs: &[Atom],
        counts: &[usize],
        ys: &[Atom],
        required: usize,
        optional: usize,
        repeated: usize,
    ) -> Result<Vec<(usize, usize)>, Issue> {
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
            let (excess, _) = owner(counts, required + optional);
            return Err(Issue::Contradiction(Contradiction::Excess(xs[excess].item)));
        }
        if extra > 0 && repeated > 1 {
            return Err(Residual::Alignment.into());
        }
        let mut pairs = Vec::new();
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
                pairs.push((owner(counts, p).0, j));
            }
            position += take;
        }
        Ok(pairs)
    }

    /// Whether some filling of the expected atoms takes one filling of the
    /// actual atoms, with `counts` items each, with every item proven to fit
    /// where it lands
    fn filled(
        &self,
        decided: &mut HashMap<(usize, usize), Status>,
        xs: &[Atom],
        counts: &[usize],
        ys: &[Atom],
    ) -> Result<bool, Issue> {
        // The expected atoms that may take the next item, skipping those that
        // need not take any
        let close = |states: &mut BTreeSet<usize>| {
            let mut j = 0;
            while j < ys.len() {
                if states.contains(&j) && ys[j].multiplicity != Multiplicity::Required {
                    states.insert(j + 1);
                }
                j += 1;
            }
        };
        let mut states = BTreeSet::from([0]);
        close(&mut states);
        for (i, &count) in counts.iter().enumerate() {
            for _ in 0..count {
                let mut next = BTreeSet::new();
                for &j in states.iter().filter(|&&j| j < ys.len()) {
                    if self.decide(decided, xs, ys, i, j)? != Status::Proven {
                        continue;
                    }
                    next.insert(match ys[j].multiplicity {
                        Multiplicity::Repeated => j,
                        _ => j + 1,
                    });
                }
                close(&mut next);
                if next.is_empty() {
                    return Ok(false);
                }
                states = next;
            }
        }
        Ok(states.contains(&ys.len()))
    }

    /// Whether an actual atom's type fits an expected atom's, decided without
    /// adding bounds; unresolved while either involves a variable
    fn decide(
        &self,
        decided: &mut HashMap<(usize, usize), Status>,
        xs: &[Atom],
        ys: &[Atom],
        i: usize,
        j: usize,
    ) -> Result<Status, Issue> {
        if let Some(&status) = decided.get(&(i, j)) {
            return Ok(status);
        }
        let status = match (self.reify(xs[i].ty), self.reify(ys[j].ty)) {
            (Ok(x), Ok(y)) => self.probe(x, y)?,
            _ => Status::Unresolved,
        };
        decided.insert((i, j), status);
        Ok(status)
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
            // The actual atoms with items of the key, in order, each with the
            // fewest and most items of the key before it
            let mut contributing = Vec::new();
            let (mut low, mut high) = (0, Some(0));
            for (i, x) in xs.iter().enumerate() {
                let is_literal = self.literal(x.key)?;
                let contributes = match is_literal {
                    true => self.same(x.key, y.key)?,
                    false => self.admits(x.key, y.key)?,
                };
                if !contributes {
                    continue;
                }
                contributing.push((x, low, high));
                let (lo, hi) = match is_literal {
                    true => {
                        claimed[i] = true;
                        range(x.multiplicity)
                    }
                    false => (0, None),
                };
                low += lo;
                high = high.zip(hi).map(|(a, b)| a + b);
            }
            let (min, max) = range(y.multiplicity);
            let excess =
                !open_expected && max.is_some_and(|max| high.is_none_or(|high| high > max));
            // More items of the key than the literal admits go to a repeated
            // domain beside it that admits the key
            let mut absorbing = Vec::new();
            if excess && overflow.is_none() {
                for domain in &domains {
                    if domain.multiplicity == Multiplicity::Repeated
                        && self.admits(domain.key, y.key)?
                    {
                        absorbing.push(*domain);
                    }
                }
            }
            // The literal takes the first items of the key, as many as it admits,
            // and the domains take the rest
            for &(x, before_low, before_high) in &contributing {
                if max.is_none_or(|max| before_low < max) {
                    self.derive(obligation, x.value, y.value, Step::Item(x.item));
                }
                let (_, hi) = match self.literal(x.key)? {
                    true => range(x.multiplicity),
                    false => (0, None),
                };
                let past = before_high.zip(hi).map(|(a, b)| a + b);
                if max.is_some_and(|max| past.is_none_or(|past| past > max)) {
                    for d in &absorbing {
                        self.derive(obligation, x.value, d.value, Step::Item(x.item));
                    }
                }
            }
            if open_expected {
                continue;
            }
            if low < min && !open_actual {
                return Err(Issue::Contradiction(Contradiction::Missing(y.item)));
            }
            if excess && absorbing.is_empty() {
                let (last, _, _) = contributing.last().unwrap();
                return Err(Issue::Contradiction(Contradiction::Excess(last.item)));
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
