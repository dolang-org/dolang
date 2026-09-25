use super::{Source, Use, solve};

fn path(u: Use, keys: &[u32]) -> Source<u32> {
    Source::Path(u, keys.to_vec())
}

#[test]
fn composition() {
    for u in [Use::NONE, Use::CO, Use::CONTRA, Use::BOTH] {
        assert_eq!(Use::CO.then(u), u);
        assert_eq!(Use::CONTRA.then(Use::CONTRA.then(u)), u);
        assert_eq!(Use::NONE.then(u), Use::NONE);
        assert_eq!(u.flip().flip(), u);
    }
    assert_eq!(Use::CONTRA.then(Use::CO), Use::CONTRA);
    assert_eq!(Use::BOTH.then(Use::CONTRA), Use::BOTH);
    assert_eq!(Use::CO.join(Use::CONTRA), Use::BOTH);
}

#[test]
fn recursion_through_itself() {
    // `List[T]`: `head -> T` and `tail -> List[T]`
    let uses = solve(&[(0, path(Use::CO, &[])), (0, path(Use::CO, &[0]))], &[0]);
    assert_eq!(uses[&0], Use::CO);

    // `Foo[T]` with only `me -> Foo[T]` is unused, so invariant
    let uses = solve(&[(0, path(Use::CO, &[0]))], &[0]);
    assert_eq!(uses[&0], Use::BOTH);
}

#[test]
fn unused_slot_is_invariant() {
    // `Wrap[T]` uses `T` only as the unused binder of `Unused[U]`
    let uses = solve(&[(1, path(Use::CO, &[0]))], &[0, 1]);
    assert_eq!(uses[&0], Use::BOTH);
    assert_eq!(uses[&1], Use::BOTH);
}

#[test]
fn join_ignores_unused() {
    // A class takes its binder's uses from a method, which does not use it, and a
    // supertype that does
    let uses = solve(&[(0, Source::Join(1)), (0, path(Use::CONTRA, &[]))], &[0]);
    assert_eq!(uses[&0], Use::CONTRA);
    assert!(!uses.contains_key(&1) || uses[&1] == Use::NONE);
}

#[test]
fn mutual_recursion() {
    // `A[T]: B[T]` and `B[U]` with `get -> A[U]` and `put x@U`
    let uses = solve(
        &[
            (0, path(Use::CO, &[1])),
            (1, path(Use::CO, &[0])),
            (1, path(Use::CONTRA, &[])),
        ],
        &[0, 1],
    );
    assert_eq!(uses[&0], Use::CONTRA);
    assert_eq!(uses[&1], Use::CONTRA);
}

/// A deterministic pseudo-random sequence
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) % u64::from(bound)) as u32
    }
}

#[test]
fn order_independent() {
    const KEYS: u32 = 8;
    let mut rng = Lcg(753);
    let uses = [Use::CO, Use::CONTRA, Use::BOTH];
    for _ in 0..200 {
        let mut constraints = Vec::new();
        for _ in 0..rng.next(12) + 1 {
            let target = rng.next(KEYS);
            let source = match rng.next(4) {
                0 => Source::Join(rng.next(KEYS)),
                _ => {
                    let keys: Vec<_> = (0..rng.next(3)).map(|_| rng.next(KEYS)).collect();
                    Source::Path(uses[rng.next(3) as usize], keys)
                }
            };
            constraints.push((target, source));
        }
        // Keys 0..4 are binders of their own declarations; the rest are captured
        let own: Vec<u32> = (0..KEYS / 2).collect();
        let expected = solve(&constraints, &own);
        let normalize = |solved: &std::collections::HashMap<u32, Use>| {
            (0..KEYS)
                .map(|key| solved.get(&key).copied().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        for _ in 0..5 {
            let mut shuffled = constraints.clone();
            for index in (1..shuffled.len()).rev() {
                shuffled.swap(index, rng.next(index as u32 + 1) as usize);
            }
            let mut own = own.clone();
            own.reverse();
            assert_eq!(normalize(&solve(&shuffled, &own)), normalize(&expected));
        }
    }
}
