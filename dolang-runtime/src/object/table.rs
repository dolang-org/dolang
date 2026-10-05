//! Fallible probing for tables whose equality callback cannot return errors.

use dolang_util::hashbrown::raw::{Bucket, RawTable};

use crate::error::Result;

pub(crate) fn find<'v, 's, T>(
    table: &RawTable<T>,
    hash: u64,
    mut eq: impl FnMut(&T) -> Result<'v, 's, bool>,
) -> Result<'v, 's, Option<Bucket<T>>> {
    let mut error = None;
    let bucket = table.find(hash, |entry| {
        if error.is_some() {
            return false;
        }
        match eq(entry) {
            Ok(equal) => equal,
            Err(err) => {
                error = Some(err);
                false
            }
        }
    });
    match error {
        Some(error) => Err(error),
        None => Ok(bucket),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{error::Error, test_support::with_vm};

    #[test]
    fn probe_stops_comparing_after_first_error() {
        with_vm(async |strand, []| {
            let mut table = RawTable::new();
            for value in 0..8 {
                table.insert(0, value, |_| 0);
            }
            let mut comparisons = 0;
            let error = find(&table, 0, |_| {
                comparisons += 1;
                Err(Error::value(strand, "first comparison failed"))
            })
            .err()
            .expect("comparison must fail");
            assert_eq!(comparisons, 1);
            assert!(format!("{}", error.display(strand)).contains("first comparison failed"));
            assert_eq!(table.len(), 8);
        });
    }
}
