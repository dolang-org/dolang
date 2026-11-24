//! The seedable `rand.Rng` generator.

use dolang::runtime::{
    Args, Error, Object, Result, Slot, Strand, Type, object::TypeBuilder, unpack,
};
use rand::{
    Rng, SeedableRng,
    rngs::{ChaCha8Rng, ChaCha12Rng, ChaCha20Rng, Xoshiro128PlusPlus, Xoshiro256PlusPlus},
};

use crate::{
    global::Tag,
    ops::{self, Source},
};

enum Algorithm {
    ChaCha8(ChaCha8Rng),
    ChaCha12(ChaCha12Rng),
    ChaCha20(ChaCha20Rng),
    Xoshiro128(Xoshiro128PlusPlus),
    Xoshiro256(Xoshiro256PlusPlus),
}

/// Seed from `seed`, or from the thread-local generator when absent.
fn seeded<R: SeedableRng>(seed: Option<u64>) -> R {
    match seed {
        Some(seed) => R::seed_from_u64(seed),
        None => R::from_rng(&mut rand::rng()),
    }
}

/// `rand.Rng`
pub(crate) struct RngObj(Algorithm);

impl RngObj {
    pub(crate) fn rng(&mut self) -> &mut dyn Rng {
        match &mut self.0 {
            Algorithm::ChaCha8(rng) => rng,
            Algorithm::ChaCha12(rng) => rng,
            Algorithm::ChaCha20(rng) => rng,
            Algorithm::Xoshiro128(rng) => rng,
            Algorithm::Xoshiro256(rng) => rng,
        }
    }
}

impl<'v> Object<'v> for RngObj {
    const NAME: &'v str = "Rng";
    const MODULE: &'v str = "rand";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Tag>();
        let syms = &global.syms;
        let (seed, algorithm) = (syms.seed, syms.algorithm);
        unpack!(strand, args => ?:seed, ?:algorithm);
        let seed = match seed {
            Some(seed) => {
                let seed = seed
                    .as_int(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected `Int`"))?;
                Some(u64::try_from(seed).map_err(|_| {
                    Error::value(strand, "expected seed to be in the range 0..2**64")
                })?)
            }
            None => None,
        };
        let algorithm = match algorithm.map(|value| value.as_sym(strand)) {
            None => Algorithm::ChaCha12(seeded(seed)),
            Some(Some(sym)) if sym == syms.chacha8 => Algorithm::ChaCha8(seeded(seed)),
            Some(Some(sym)) if sym == syms.chacha12 => Algorithm::ChaCha12(seeded(seed)),
            Some(Some(sym)) if sym == syms.chacha20 => Algorithm::ChaCha20(seeded(seed)),
            Some(Some(sym)) if sym == syms.xoshiro128pp => Algorithm::Xoshiro128(seeded(seed)),
            Some(Some(sym)) if sym == syms.xoshiro256pp => Algorithm::Xoshiro256(seeded(seed)),
            Some(_) => {
                return Err(Error::value(
                    strand,
                    "algorithm must be :CHACHA8:, :CHACHA12:, :CHACHA20:, :XOSHIRO128PP:, or :XOSHIRO256PP:",
                ));
            }
        };
        this.create(strand, RngObj(algorithm), out);
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("int", async move |this, strand, args, out| {
                ops::int(strand, Source::Rng(this), args, out)
            })
            .method("float", async move |this, strand, args, out| {
                ops::float(strand, Source::Rng(this), args, out)
            })
            .method("bool", async move |this, strand, args, out| {
                ops::bool(strand, Source::Rng(this), args, out)
            })
            .method("bin", async move |this, strand, args, out| {
                ops::bin(strand, Source::Rng(this), args, out)
            })
            .method("str", async move |this, strand, args, out| {
                let global = strand.state::<Tag>();
                ops::str(strand, &global, Source::Rng(this), args, out)
            })
            .method("pick", async move |this, strand, args, out| {
                let global = strand.state::<Tag>();
                ops::pick(strand, &global, Source::Rng(this), args, out)
            })
            .method("sample", async move |this, strand, args, out| {
                let global = strand.state::<Tag>();
                ops::sample(strand, &global, Source::Rng(this), args, out)
            })
            .method("shuffle", async move |this, strand, args, _out| {
                ops::shuffle(strand, Source::Rng(this), args)
            })
    }
}
