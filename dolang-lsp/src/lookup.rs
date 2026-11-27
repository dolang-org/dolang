//! Declarations in the modules a check saw, found by module and name.
//!
//! A check keeps the units of the modules it read from source, and of those it
//! took as typelibs written of their units, so what's found is what the
//! check's analysis was computed against.  A module it read from a bundled
//! typelib, or from a module's last good typelib, has no source here, and is
//! left to the static doc index.

use std::{collections::HashMap, sync::Arc};

use dolang_compile::{Kind, NodeId, Unit};
use tower_lsp_server::ls_types::Uri;

/// A module a check read from source, or took as a typelib written of `unit`
pub(crate) struct Module {
    pub(crate) uri: Uri,
    pub(crate) unit: Arc<Unit<'static>>,
}

/// The modules a check has the units of, by name
pub(crate) type Modules = HashMap<String, Module>;

/// Where a lookup among a check's modules ended
pub(crate) type Target<'m> = dolang_ide::Target<'m, &'m Module>;

/// Finds a module's unit among `modules`, as the shared lookups take it
pub(crate) fn resolver<'m>(
    modules: &'m Modules,
) -> impl Fn(&str) -> Option<(&'m Unit<'m>, &'m Module)> + 'm {
    move |name| modules.get(name).map(|module| (&*module.unit, module))
}

/// A declaration other modules may name, by its module's name: an item the
/// module declares, or a member of a class it declares.  It's found by name
/// rather than by node, since checks may hold different revisions of its
/// module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Name {
    pub(crate) module: String,
    /// The item, or the class declaring the member
    pub(crate) item: String,
    pub(crate) member: Option<String>,
}

impl Name {
    /// What other modules name the declaration `id` of `unit`, the source of
    /// `module`, if they may name it.  An `@def` overload is named as its
    /// implementation is.
    pub(crate) fn of(unit: &Unit<'_>, module: &str, id: NodeId) -> Option<Self> {
        let node = unit.node(id)?;
        let name = unit.text(node.definition()?).to_owned();
        let parent = node.parent()?;
        let top = Some(parent) == unit.root();
        let (item, member) = match node.kind() {
            Kind::Class { .. } | Kind::Bind { .. } | Kind::Alias { .. } | Kind::Function { .. }
                if top =>
            {
                (name, None)
            }
            Kind::Method { .. } | Kind::SpecialMethod { .. } | Kind::Field { .. } => {
                let class = unit.node(parent)?;
                if !matches!(class.kind(), Kind::Class { .. }) || class.parent() != unit.root() {
                    return None;
                }
                (unit.text(class.definition()?).to_owned(), Some(name))
            }
            _ => return None,
        };
        Some(Self {
            module: module.to_owned(),
            item,
            member,
        })
    }

    /// What other modules name the declaration a lookup found.
    pub(crate) fn of_target<M>(target: &dolang_ide::Target<'_, M>) -> Option<Self> {
        match target {
            dolang_ide::Target::Source { unit, name, id, .. } => Self::of(unit, name, *id),
            dolang_ide::Target::Bundled { module, item } => {
                let (item, member) = match item.split_once('.') {
                    Some((class, member)) => (class.to_owned(), Some(member.to_owned())),
                    None => (item.clone(), None),
                };
                Some(Name {
                    module: module.clone(),
                    item,
                    member,
                })
            }
        }
    }

    /// Where the declaration is among `modules`.
    pub(crate) fn find<'m>(&self, modules: &'m Modules) -> Option<Target<'m>> {
        let resolve = resolver(modules);
        match &self.member {
            None => dolang_ide::item(resolve, &self.module, &self.item),
            Some(name) => dolang_ide::member(resolve, &self.module, &self.item, name),
        }
    }
}
