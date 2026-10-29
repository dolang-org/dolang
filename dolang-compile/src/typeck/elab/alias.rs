//! Alias heads: what each transparent alias stands for at its root, found by
//! following its chain of aliases. An alias whose chain comes back to it is
//! reported, and every alias on the cycle stands for an error.

use rustc_hash::FxHashMap;

use super::{AliasCycle, DeclNode, Diag, Head, Referent, Tables, UnitDiag, surface::TypeExpr};
use crate::typeck::r#type::{DeclId, DeclKind};

/// Find the head of every transparent alias.
pub(crate) fn aliases(tables: &mut Tables<'_>, diags: &mut Vec<UnitDiag>) {
    let mut aliases = Aliases {
        tables,
        heads: FxHashMap::default(),
        diags,
    };
    // Every alias is followed in declaration order, so which one a cycle is reported
    // at does not depend on which alias names it first
    for id in tables.decl_ids() {
        if tables.decl(id).kind == DeclKind::Alias {
            aliases.head(id);
        }
    }
    let heads = aliases.heads;
    for (id, head) in heads {
        let head = head.expect("every alias is finished");
        tables.facts_mut(id).aliases.insert(id, head);
    }
}

/// The heads of transparent aliases, found by following each alias's chain
struct Aliases<'a, 'u> {
    tables: &'a Tables<'u>,
    /// Each alias's head, or `None` while its chain is being followed
    heads: FxHashMap<DeclId, Option<Head>>,
    diags: &'a mut Vec<UnitDiag>,
}

impl Aliases<'_, '_> {
    fn head(&mut self, id: DeclId) -> Head {
        let decl = self.tables.decl(id);
        match self.heads.get(&id) {
            Some(Some(head)) => return head.clone(),
            // Every alias on the cycle takes the error this returns, so it is
            // reported once
            Some(None) => {
                let span = decl.name_span().expect("an alias is named");
                self.diags.push((decl.unit, Diag::new(AliasCycle(span))));
                return Head::Error;
            }
            None => {}
        }
        self.heads.insert(id, None);
        let DeclNode::Alias(alias) = &decl.node else {
            unreachable!("only an alias has a head")
        };
        let body = alias.body.expect("an opaque alias is not transparent");
        let head = self.ty(&self.tables.site(body).ty);
        self.heads.insert(id, Some(head.clone()));
        head
    }

    fn ty(&mut self, ty: &TypeExpr) -> Head {
        match ty {
            TypeExpr::Group { ty, .. } => self.ty(ty),
            TypeExpr::App { base, .. } => self.ty(base),
            TypeExpr::Name(head) => match self.tables.linked(head.link) {
                Some(Referent::Decl(decl)) if self.tables.decl(*decl).kind == DeclKind::Alias => {
                    self.head(*decl)
                }
                Some(Referent::Decl(decl)) => Head::Decl(*decl),
                Some(Referent::Binder(binder)) => Head::Binder(*binder),
                Some(Referent::Module(_) | Referent::Value(_) | Referent::Error) | None => {
                    Head::Error
                }
            },
            TypeExpr::Const { .. }
            | TypeExpr::Union { .. }
            | TypeExpr::Func { .. }
            | TypeExpr::Schema { .. }
            | TypeExpr::Tuple { .. }
            | TypeExpr::Record { .. }
            | TypeExpr::Array { .. } => Head::Structural,
            TypeExpr::Error { .. } => Head::Error,
        }
    }
}
