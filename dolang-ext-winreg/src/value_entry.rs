//! `winreg.Value` — one entry of a [`crate::values::Values`] snapshot.

use dolang::runtime::{Object, Output, Result, Slot, State, Strand, object::TypeBuilder};
use dolang_vfs_winreg::Value;

use crate::{convert, global::Global};

pub(crate) struct ValueEntry;

pub(crate) struct ValueEntryAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) name: String,
    pub(crate) value: Value,
}

fn kind_to_do<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let sym = match value {
        Value::Sz(_) => global.syms.sz,
        Value::ExpandSz(_) => global.syms.expand_sz,
        Value::MultiSz(_) => global.syms.multi_sz,
        Value::Dword(_) => global.syms.dword,
        Value::DwordBigEndian(_) => global.syms.dword_big_endian,
        Value::Qword(_) => global.syms.qword,
        Value::Binary(_) => global.syms.binary,
        Value::None => global.syms.none,
        Value::Other { kind, .. } => {
            Output::set(strand, out, i128::from(*kind));
            return Ok(());
        }
    };
    Output::set(strand, out, sym);
    Ok(())
}

impl<'v> Object<'v> for ValueEntry {
    const NAME: &'v str = "Value";
    const MODULE: &'v str = "winreg";
    type Annex = ValueEntryAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().name.as_str());
                Ok(())
            })
            .get("kind", |this, strand, out| {
                let annex = this.annex();
                kind_to_do(strand, annex.global, &annex.value, out)
            })
            .get("value", |this, strand, out| {
                convert::to_do(strand, &this.annex().value, out)
            })
    }
}
