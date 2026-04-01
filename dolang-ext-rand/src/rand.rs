use dolang::runtime::{Error, Output, Result, Strand, unpack, value::Value, vm::Builder};
use rand::RngExt;

const DEFAULT_ALPHABET: &str = "_-0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn require_i64<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    msg: &'static str,
) -> Result<'v, 's, i64> {
    value
        .as_i64(strand)
        .ok_or_else(|| Error::type_error(strand, msg))
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let alphabet_key = builder.sym("alphabet");

    builder
        .module("rand")
        .function("int", async move |strand, args, out| {
            let ([start], [end]) = unpack!(strand, args, 1, 1)?;
            let start = require_i64(strand, &start, "expected `int`")?;
            let (start, end) = match end {
                Some(end) => (start, require_i64(strand, &end, "expected `int`")?),
                None => (0, start),
            };
            if end <= start {
                return Err(Error::value(
                    strand,
                    "expected end to be greater than start",
                ));
            }
            let mut rng = rand::rng();
            Output::set(strand, out, rng.random_range(start..end));
            Ok(())
        })
        .function("string", async move |strand, args, out| {
            let ([len], [alphabet]) = unpack!(strand, args, 1, 0, alphabet_key = None)?;
            let len = require_i64(strand, &len, "expected `int`")?;
            let len = usize::try_from(len)
                .map_err(|_| Error::value(strand, "expected len to be non-negative"))?;
            let alphabet = match &alphabet {
                Some(alphabet) => alphabet
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected `str`"))?,
                None => DEFAULT_ALPHABET,
            };
            if alphabet.is_empty() {
                return Err(Error::value(strand, "expected alphabet to be non-empty"));
            }
            let chars = alphabet.chars().collect::<Vec<_>>();
            let mut rng = rand::rng();
            let text = (0..len)
                .map(|_| chars[rng.random_range(0..chars.len())])
                .collect::<String>();
            Output::set(strand, out, text.as_str());
            Ok(())
        })
        .function("pick", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let array = value
                .as_array(strand.vm())
                .ok_or_else(|| Error::type_error(strand, "expected array"))?;
            let len = array.len(strand)?;
            if len == 0 {
                return Err(Error::value(strand, "expected non-empty array"));
            }
            let mut rng = rand::rng();
            let index = rng.random_range(0..len);
            array.get(strand, index, out)?;
            Ok(())
        })
        .function_with_slots(
            "shuffle",
            async move |strand, args, _out, [mut left, mut right]| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let arr = value
                    .as_array(strand.vm())
                    .ok_or_else(|| Error::type_error(strand, "expected array"))?;
                let len = arr.len(strand)?;
                let mut rng = rand::rng();
                for i in (1..len).rev() {
                    let j = rng.random_range(0..=i);
                    if i == j {
                        continue;
                    }
                    arr.get(strand, i, &mut left)?;
                    arr.get(strand, j, &mut right)?;
                    arr.set(strand, i, &mut right)?;
                    arr.set(strand, j, &mut left)?;
                }
                Ok(())
            },
        )
        .commit();
}
