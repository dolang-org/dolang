//! A textual dump of a graph, for tests and debugging. Variables are shown by
//! their source names, with synthetic ones numbered.

use std::fmt::{self, Write};

use super::{
    Against, Collection, Expr, ExprKind, FmtSpec, FuncKind, Ir, Item, Member, Origin, Pattern,
    PatternKey, Relation, Step, Tag, Target, Terminal, VarId,
};
use crate::{
    source::Span,
    typeck::{
        elab::ModuleRef,
        r#type::{Database, Literal},
    },
};

impl Ir {
    /// Dump the graph, naming variables by the source text of their spans
    pub(crate) fn dump<'s>(&self, db: &Database, text: impl Fn(Span) -> &'s str) -> String {
        let dump = Dump {
            ir: self,
            db,
            text: &text,
        };
        let mut out = String::new();
        dump.write(&mut out).expect("writing to a string succeeds");
        out
    }
}

struct Dump<'a, 's> {
    ir: &'a Ir,
    db: &'a Database,
    text: &'a dyn Fn(Span) -> &'s str,
}

impl Dump<'_, '_> {
    fn write(&self, out: &mut String) -> fmt::Result {
        for (id, func) in self.ir.funcs() {
            write!(out, "f{}", id.index())?;
            match func.kind {
                FuncKind::Module(_) => write!(out, " module")?,
                FuncKind::Decl(decl) => write!(out, " decl{}", decl.index())?,
            }
            if let Some(parent) = func.parent {
                write!(out, " in f{}", parent.index())?;
            }
            write!(
                out,
                ": entry b{}, exit b{}",
                func.entry.index(),
                func.exit.index()
            )?;
            write!(out, ", params ")?;
            self.pattern(out, &func.params)?;
            if let Some(signature) = &func.signature {
                write!(out, ", signature (")?;
                for (index, &param) in signature.params.iter().enumerate() {
                    if index != 0 {
                        write!(out, ", ")?;
                    }
                    self.slot(out, param)?;
                }
                write!(out, ") <")?;
                self.slot(out, signature.input)?;
                write!(out, " >")?;
                self.slot(out, signature.output)?;
                write!(out, " -> ")?;
                self.slot(out, signature.result)?;
            }
            if !func.captures.is_empty() {
                write!(out, ", captures")?;
                for &var in &func.captures {
                    write!(out, " ")?;
                    self.var(out, var)?;
                    if self.ir.var(var).volatile {
                        write!(out, "!")?;
                    }
                }
            }
            let bottom: Vec<_> = (func.vars.iter())
                .filter(|&&var| self.ir.var(var).bottom)
                .collect();
            if !bottom.is_empty() {
                write!(out, ", bottom")?;
                for &var in bottom {
                    write!(out, " ")?;
                    self.var(out, var)?;
                }
            }
            writeln!(out)?;
        }
        for (id, block) in self.ir.blocks() {
            write!(out, "b{} f{}", id.index(), block.func.index())?;
            if let Some(handler) = block.handler {
                write!(out, " handler b{}", handler.index())?;
            }
            if block.depth != 0 {
                write!(out, " depth {}", block.depth)?;
            }
            writeln!(out, ":")?;
            for step in &block.steps {
                write!(out, "  ")?;
                self.step(out, step)?;
                writeln!(out)?;
            }
            write!(out, "  ")?;
            self.terminal(out, &block.terminal)?;
            writeln!(out)?;
        }
        Ok(())
    }

    fn var(&self, out: &mut String, var: VarId) -> fmt::Result {
        match self.ir.var(var).origin {
            Origin::Source(span) => write!(out, "{}", (self.text)(span)),
            Origin::Result => write!(out, "result{}", self.ir.var(var).owner.index()),
            Origin::Synthetic | Origin::Field(_) | Origin::Signature => {
                write!(out, "t{}", var.index())
            }
        }
    }

    /// A signature's variable, or `_` for an annotated item
    fn slot(&self, out: &mut String, var: Option<VarId>) -> fmt::Result {
        match var {
            Some(var) => self.var(out, var),
            None => write!(out, "_"),
        }
    }

    fn step(&self, out: &mut String, step: &Step) -> fmt::Result {
        match step {
            Step::Let { pattern, value } => {
                write!(out, "let ")?;
                self.pattern(out, pattern)?;
                write!(out, " = ")?;
                self.expr(out, value)
            }
            Step::Assign { target, value } => {
                match target {
                    Target::Var(var) => self.var(out, *var)?,
                    Target::Field { object, member, .. } => {
                        self.expr(out, object)?;
                        self.member(out, member)?;
                    }
                    Target::Index { object, index, .. } => {
                        self.expr(out, object)?;
                        write!(out, "[")?;
                        self.expr(out, index)?;
                        write!(out, "]")?;
                    }
                }
                write!(out, " = ")?;
                self.expr(out, value)
            }
            Step::Default { var, value } => {
                write!(out, "default ")?;
                self.var(out, *var)?;
                write!(out, " = ")?;
                self.expr(out, value)
            }
            Step::Eval(value) => {
                write!(out, "eval ")?;
                self.expr(out, value)
            }
            Step::Push(value) => {
                write!(out, "push ")?;
                self.expr(out, value)
            }
            Step::Dup => write!(out, "dup"),
            Step::Pop => write!(out, "pop"),
            Step::Assume(assume) => {
                write!(out, "assume ")?;
                self.var(out, assume.var)?;
                let relation = match (assume.relation, assume.negated) {
                    (Relation::Upper, false) => "<:",
                    (Relation::Upper, true) => "!<:",
                    (Relation::Exact, false) => "==",
                    (Relation::Exact, true) => "!=",
                };
                write!(out, " {relation} ")?;
                match &assume.against {
                    Against::Class(class) => {
                        write!(out, "class ")?;
                        self.expr(out, class)
                    }
                    Against::Value(value) => self.expr(out, value),
                    Against::Decl(decl) => write!(out, "class{}", decl.index()),
                    Against::Type(ty) => write!(out, "type{}", ty.index()),
                }
            }
        }
    }

    fn terminal(&self, out: &mut String, terminal: &Terminal) -> fmt::Result {
        match terminal {
            Terminal::Branch(block) => write!(out, "goto b{}", block.index()),
            Terminal::If { cond, then, else_ } => {
                write!(out, "if ")?;
                self.expr(out, cond)?;
                write!(out, " then b{} else b{}", then.index(), else_.index())
            }
            Terminal::Unpack {
                pattern,
                value,
                then,
                else_,
            } => {
                write!(out, "unpack ")?;
                self.pattern(out, pattern)?;
                write!(out, " = ")?;
                self.expr(out, value)?;
                write!(out, " then b{} else b{}", then.index(), else_.index())
            }
            Terminal::Catch { clauses, otherwise } => {
                write!(out, "catch")?;
                for (class, block) in clauses {
                    write!(out, " ")?;
                    self.expr(out, class)?;
                    write!(out, " -> b{},", block.index())?;
                }
                write!(out, " else b{}", otherwise.index())
            }
            Terminal::Next {
                iter,
                pattern,
                body,
                exit,
                ..
            } => {
                write!(out, "next ")?;
                self.pattern(out, pattern)?;
                write!(out, " in ")?;
                self.var(out, *iter)?;
                write!(out, " then b{} else b{}", body.index(), exit.index())
            }
            Terminal::Return => write!(out, "return"),
            Terminal::Throw(value) => {
                write!(out, "throw ")?;
                self.expr(out, value)
            }
            Terminal::Leave { entry, tag } => {
                write!(out, "leave to b{} then ", entry.index())?;
                match tag {
                    Tag::Goto(block) => write!(out, "b{}", block.index()),
                    Tag::Rethrow => write!(out, "rethrow"),
                }
            }
            Terminal::EndFinally => write!(out, "end finally"),
            Terminal::Guard { next, targets } => {
                write!(out, "guard b{}", next.index())?;
                for target in targets {
                    write!(out, " | b{}", target.index())?;
                }
                Ok(())
            }
            Terminal::Escape => write!(out, "escape"),
            Terminal::ReturnFrom { func, value } => {
                write!(out, "return from f{} ", func.index())?;
                self.expr(out, value)
            }
            Terminal::Unreachable => write!(out, "unreachable"),
        }
    }

    fn pattern(&self, out: &mut String, pattern: &Pattern) -> fmt::Result {
        let items = match pattern {
            Pattern::Bind(var) => return self.var(out, *var),
            Pattern::Unpack(items) => items,
        };
        write!(out, "(")?;
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                write!(out, ", ")?;
            }
            match &item.key {
                PatternKey::Pos => {}
                PatternKey::Key(name) => write!(out, "{}: ", self.db.symbol(*name))?,
                PatternKey::ConstKey(key) => {
                    write!(out, "(")?;
                    self.expr(out, key)?;
                    write!(out, "): ")?;
                }
                PatternKey::Rest(kind) => write!(out, "{kind:?}...")?,
            }
            match item.var {
                Some(var) => self.var(out, var)?,
                None => write!(out, "_")?,
            }
        }
        write!(out, ")")
    }

    fn member(&self, out: &mut String, member: &Member) -> fmt::Result {
        let name = self.db.symbol(member.key.name);
        match (member.key.special, member.key.private) {
            (true, _) => write!(out, ".({name})"),
            (false, true) => write!(out, ".#{name}"),
            (false, false) => write!(out, ".{name}"),
        }
    }

    fn exprs(&self, out: &mut String, exprs: &[Expr]) -> fmt::Result {
        for (index, expr) in exprs.iter().enumerate() {
            if index != 0 {
                write!(out, ", ")?;
            }
            self.expr(out, expr)?;
        }
        Ok(())
    }

    fn items(&self, out: &mut String, items: &[Item]) -> fmt::Result {
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                write!(out, ", ")?;
            }
            match item {
                Item::Pos(value) => self.expr(out, value)?,
                Item::Key(name, value) => {
                    write!(out, "{}: ", self.db.symbol(*name))?;
                    self.expr(out, value)?;
                }
                Item::Pair(key, value) => {
                    self.expr(out, key)?;
                    write!(out, " => ")?;
                    self.expr(out, value)?;
                }
                Item::Spread(value) => {
                    write!(out, "...")?;
                    self.expr(out, value)?;
                }
                Item::For { items, .. } => {
                    write!(out, "for {{")?;
                    self.items(out, items)?;
                    write!(out, "}}")?;
                }
                Item::If { then, else_, .. } => {
                    write!(out, "if {{")?;
                    self.items(out, then)?;
                    write!(out, "}} else {{")?;
                    self.items(out, else_)?;
                    write!(out, "}}")?;
                }
            }
        }
        Ok(())
    }

    fn literal(&self, out: &mut String, literal: &Literal) -> fmt::Result {
        match literal {
            Literal::Nil => write!(out, "nil"),
            Literal::Bool(value) => write!(out, "{value}"),
            Literal::Int(value) => write!(out, "{value}"),
            Literal::Str(value) => write!(out, "{value:?}"),
            Literal::Sym(name) => write!(out, ":{}:", self.db.symbol(*name)),
        }
    }

    fn expr(&self, out: &mut String, expr: &Expr) -> fmt::Result {
        match &expr.kind {
            ExprKind::Literal(literal) => self.literal(out, literal),
            ExprKind::Float => write!(out, "<float>"),
            ExprKind::Bin => write!(out, "<bin>"),
            ExprKind::Concat(parts) => {
                write!(out, "concat(")?;
                self.exprs(out, parts)?;
                write!(out, ")")
            }
            ExprKind::BinConcat { parts, .. } => {
                write!(out, "bin_concat(")?;
                self.exprs(out, parts)?;
                write!(out, ")")
            }
            ExprKind::Fmt(parts) => {
                write!(out, "fmt(")?;
                self.exprs(out, parts)?;
                write!(out, ")")
            }
            ExprKind::FmtValue { value, spec, .. } => {
                write!(out, "fmt_value(")?;
                self.expr(out, value)?;
                self.spec(out, spec, true)?;
                write!(out, ")")
            }
            ExprKind::FmtParam { name, spec, .. } => {
                write!(out, "fmt_param(")?;
                self.literal(out, name)?;
                self.spec(out, spec, true)?;
                write!(out, ")")
            }
            ExprKind::Var(var) | ExprKind::Copy(var) => self.var(out, *var),
            ExprKind::Class(decl) => write!(out, "class{}", decl.index()),
            ExprKind::Import { module, item } => {
                match module {
                    ModuleRef::Unit(unit) => write!(out, "<unit{}>", unit.index())?,
                    ModuleRef::External(name) => write!(out, "{name}")?,
                }
                if let Some(item) = item {
                    write!(out, "::{}", self.db.symbol(*item))?;
                }
                Ok(())
            }
            ExprKind::Lambda(func) => write!(out, "f{}", func.index()),
            ExprKind::Call { callee, args, .. } => {
                self.expr(out, callee)?;
                write!(out, "(")?;
                self.items(out, args)?;
                write!(out, ")")
            }
            ExprKind::Invoke {
                receiver,
                member,
                args,
                ..
            } => {
                self.expr(out, receiver)?;
                self.member(out, member)?;
                write!(out, "(")?;
                self.items(out, args)?;
                write!(out, ")")
            }
            ExprKind::Get { object, member, .. } => {
                self.expr(out, object)?;
                self.member(out, member)
            }
            ExprKind::Index { object, index, .. } => {
                self.expr(out, object)?;
                write!(out, "[")?;
                self.expr(out, index)?;
                write!(out, "]")
            }
            ExprKind::Unary { op, operand, .. } => {
                write!(out, "({op}")?;
                self.expr(out, operand)?;
                write!(out, ")")
            }
            ExprKind::Binary { op, operands, .. } => {
                write!(out, "(")?;
                self.expr(out, &operands[0])?;
                write!(out, " {op} ")?;
                self.expr(out, &operands[1])?;
                write!(out, ")")
            }
            ExprKind::Range { bounds, .. } => {
                write!(out, "(")?;
                if let Some(start) = &bounds[0] {
                    self.expr(out, start)?;
                }
                write!(out, "..")?;
                if let Some(end) = &bounds[1] {
                    self.expr(out, end)?;
                }
                write!(out, ")")
            }
            ExprKind::Collection { kind, items, .. } => {
                let kind = match kind {
                    Collection::Array => "array",
                    Collection::Dict => "dict",
                    Collection::Tuple => "tuple",
                    Collection::Record => "record",
                };
                write!(out, "{kind}[")?;
                self.items(out, items)?;
                write!(out, "]")
            }
            ExprKind::AmbientInput => write!(out, "<input>"),
            ExprKind::Operand => write!(out, "<pop>"),
            ExprKind::Never => write!(out, "never"),
            ExprKind::Namespace => write!(out, "<namespace>"),
            ExprKind::TypeTest { value, class } => {
                write!(out, "type_test(")?;
                self.expr(out, value)?;
                write!(out, ", {class:?})")
            }
            ExprKind::Error => write!(out, "<error>"),
        }
    }

    /// The evaluated parts of a specification, as keyword arguments following others
    /// if `after`
    fn spec(&self, out: &mut String, spec: &FmtSpec, mut after: bool) -> fmt::Result {
        for (name, count) in [("width", &spec.width), ("precision", &spec.precision)] {
            if let Some(count) = count {
                if after {
                    write!(out, ", ")?;
                }
                write!(out, "{name}: ")?;
                self.expr(out, count)?;
                after = true;
            }
        }
        Ok(())
    }
}
