use std::{
    cell::Cell,
    collections::HashMap,
    fmt::{self, Write},
    result,
};

use dolang_util::{arena::ArenaVec, intern::BinTable};

use crate::{
    Compiler, Mode, PreludeImport,
    ast::{
        self, Arg, ArrayElem, Assign, Bind, Block, Class, Def, DictElem, Expand, Expr, ExprBody,
        For, Function, GetVariant, Ident, If, Import, ImportElement, ImportItem, Key, LValue, Let,
        Method, NlGuard, NlInfo, Origin, Pair, Param, Pattern, PatternBind, PrimStmt, Res, Return,
        Root, SideEffect, Single, Stmt, Try, Var, While, visit::Node,
    },
    diag::{AnnotationKind, Severity},
    source::{Annotate, Diagnose, Diags, File, Patch, Span},
    sym,
};

struct Unbound(Span);

impl Diagnose for Unbound {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "unbound identifier")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct DuplicateMemberScope(Span);

impl Diagnose for DuplicateMemberScope {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "a field may have only one `class` or `static` decorator")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct UnsupportedFieldDecorator(Span);

impl Diagnose for UnsupportedFieldDecorator {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "only the prelude `class` and `static` decorators are supported on a field"
        )
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct BadBreak(Span);

impl Diagnose for BadBreak {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "break outside of loop")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct InappropriatePub(Span);

impl Diagnose for InappropriatePub {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "`pub` may only be used at the top level")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct BadContinue(Span);

impl Diagnose for BadContinue {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "continue outside of loop")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct BadReturn(Span);

impl Diagnose for BadReturn {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "return at top level of REPL")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct IfWithoutElse(Span);

impl Diagnose for IfWithoutElse {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "`if` without `else` always evaluates to `nil`")
    }

    fn span(&self) -> Span {
        self.0
    }
}

#[derive(Clone)]
struct BadNl {
    span: Span,
    lambda_span: Span,
}

impl Diagnose for BadNl {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "non-local {} not allowed", compiler.file.str(self.span))
    }

    fn span(&self) -> Span {
        self.span
    }

    fn annotations(&self) -> Box<dyn Iterator<Item = Box<dyn Annotate>>> {
        Box::new([Box::new(self.clone()) as Box<dyn Annotate>].into_iter())
    }
}

impl Annotate for BadNl {
    fn kind(&self) -> AnnotationKind {
        AnnotationKind::Context
    }

    #[expect(clippy::misnamed_getters)]
    fn span(&self) -> Span {
        self.lambda_span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "this closure is not in argument position")
    }
}

struct Unreachable(Span);

impl Diagnose for Unreachable {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "unreachable statement")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct UnusedVar(Span);

impl Diagnose for UnusedVar {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "unused variable")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct Uncallable {
    span: Span,
    expr_span: Span,
}

impl Diagnose for Uncallable {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "attempt to call non-function value")
    }

    fn span(&self) -> Span {
        self.span
    }

    fn annotations(&self) -> Box<dyn Iterator<Item = Box<dyn Annotate>>> {
        let annotation = Box::new(UncallableHead {
            span: self.expr_span,
        }) as Box<dyn Annotate>;
        Box::new([annotation].into_iter())
    }
}

struct UncallableHead {
    span: Span,
}

impl Annotate for UncallableHead {
    fn span(&self) -> Span {
        self.span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "this expression is never a function")
    }

    fn kind(&self) -> AnnotationKind {
        AnnotationKind::Context
    }
}

struct BinaryOpAsArg {
    span: Span,
    operator_span: Span,
}

impl Diagnose for BinaryOpAsArg {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "function call where expression may be intended")
    }

    fn span(&self) -> Span {
        self.span
    }

    fn annotations(&self) -> Box<dyn Iterator<Item = Box<dyn Annotate>>> {
        Box::new(
            [Box::new(BinaryOpAnnotation {
                span: self.operator_span,
            }) as Box<dyn Annotate>]
            .into_iter(),
        )
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(BinaryOpPatch { span: self.span }) as Box<dyn Patch>].into_iter())
    }
}

struct BinaryOpPatch {
    span: Span,
}

impl Patch for BinaryOpPatch {
    fn span(&self) -> Span {
        self.span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "wrap entire expression in parentheses")
    }

    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let original_text = compiler.file.str(self.span);
        write!(w, "({})", original_text)
    }
}

struct BinaryOpAnnotation {
    span: Span,
}

impl Annotate for BinaryOpAnnotation {
    fn span(&self) -> Span {
        self.span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "this token is a literal string")
    }

    fn kind(&self) -> AnnotationKind {
        AnnotationKind::Context
    }
}

// Warning: statement with no effect (pure constant)
struct NoEffect {
    span: Span,
}

impl Diagnose for NoEffect {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "statement with no effect")
    }

    fn span(&self) -> Span {
        self.span
    }

    fn notes(&self) -> Box<dyn Iterator<Item = Box<dyn crate::source::Note>>> {
        Box::new([Box::new(NoEffectNote) as Box<dyn crate::source::Note>].into_iter())
    }
}

struct NoEffectNote;

impl crate::source::Note for NoEffectNote {
    fn kind(&self) -> crate::diag::NoteKind {
        crate::diag::NoteKind::Help
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "considering removing this statement")
    }
}

// Warning: statement with no effect (variable reference)
#[derive(Clone)]
struct NoEffectVar {
    span: Span,
    expr_span: Span,
}

impl Diagnose for NoEffectVar {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "statement with no effect")
    }

    fn span(&self) -> Span {
        self.span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(self.clone()) as Box<dyn Patch>].into_iter())
    }
}

impl Patch for NoEffectVar {
    #[expect(clippy::misnamed_getters)]
    fn span(&self) -> Span {
        self.expr_span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "add () to make this a call")
    }

    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let original = compiler.file.str(self.expr_span);
        write!(w, "{}()", original)
    }
}

// Warning: statement with no apparent effect (operations on variables)
#[derive(Clone)]
struct NoApparentEffect {
    span: Span,
    expr_span: Span,
}

impl Diagnose for NoApparentEffect {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "statement with no apparent effect")
    }

    fn span(&self) -> Span {
        self.span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(self.clone()) as Box<dyn Patch>].into_iter())
    }
}

impl Patch for NoApparentEffect {
    #[expect(clippy::misnamed_getters)]
    fn span(&self) -> Span {
        self.expr_span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "bind result to _ to suppress warning")
    }

    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let original = compiler.file.str(self.expr_span);
        write!(w, "let _ = {}", original)
    }
}

// Warning: discarded computation with inner side effects
#[derive(Clone)]
struct DiscardedComputation {
    span: Span,
    expr_span: Span,
}

impl Diagnose for DiscardedComputation {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "result of computation discarded")
    }

    fn span(&self) -> Span {
        self.span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(self.clone()) as Box<dyn Patch>].into_iter())
    }
}

impl Patch for DiscardedComputation {
    #[expect(clippy::misnamed_getters)]
    fn span(&self) -> Span {
        self.expr_span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "bind result to _ to suppress warning")
    }

    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let original = compiler.file.str(self.expr_span);
        write!(w, "let _ = {}", original)
    }
}

struct NoPrivateField {
    span: Span,
    name: String,
}

impl Diagnose for NoPrivateField {
    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "no private field `{}` in scope", self.name)
    }

    fn span(&self) -> Span {
        self.span
    }
}

#[derive(Copy, Clone)]
struct PrivateFieldWithoutHash {
    span: Span,
}

impl Patch for PrivateFieldWithoutHash {
    fn span(&self) -> Span {
        Span {
            start: self.span.start,
            end: self.span.start,
        }
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "use `.#` to access private field")
    }

    fn sub(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "#")
    }
}

impl Diagnose for PrivateFieldWithoutHash {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "`{}` is private", compiler.file.str(self.span))
    }

    fn span(&self) -> Span {
        self.span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new([Box::new(*self) as Box<dyn Patch>].into_iter())
    }
}

type Epoch = u32;

pub(crate) struct Elaborater<'a> {
    mode: Mode<'a>,
    file: &'a File<'a>,
    diags: &'a Diags,
    bintab: &'a mut BinTable,
    symtab: &'a mut sym::Table,
    fail: bool,
    epoch: Epoch,
}

enum ScopeKind {
    Normal,
    Lambda,
    Function,
    Loop,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum CanBranch {
    No,
    BadNl(Span),
    Yes,
}

impl CanBranch {
    fn bad_nl(self, span: Option<Span>) -> Self {
        match (self, span) {
            (Self::Yes, Some(span)) => Self::BadNl(span),
            (other, _) => other,
        }
    }
}

enum Scope<'s> {
    Base,
    Nested {
        kind: ScopeKind,
        can_break: CanBranch,
        can_continue: CanBranch,
        can_return: CanBranch,
        nl_break: Cell<bool>,
        nl_continue: Cell<bool>,
        nl_return: Cell<Option<usize>>,
        vars: ArenaVec<Cell<(Var, Epoch)>>,
        parent: &'s Scope<'s>,
        index: HashMap<sym::Id, usize>,
    },
    Class {
        parent: &'s Scope<'s>,
        class_private: HashMap<String, sym::Id>,
    },
}

impl<'s> Scope<'s> {
    fn should_warn_unused(&self, resolver: &Elaborater, var: &Var) -> Option<Span> {
        if var.used
            || var.exported
            || (matches!(resolver.mode, Mode::Repl) && self.is_top_level())
            || self.is_class()
        {
            return None;
        }
        // Prelude and elaborator-invented bindings are not written by the user,
        // so leaving one unused is not something to warn about; neither has a
        // name in the source, so having one is the test.
        let span = var.origin.name()?;
        if resolver.file.str(span).starts_with('_') {
            return None;
        }
        Some(span)
    }
    fn new() -> Self {
        Self::Base
    }

    fn can_break(&'s self) -> CanBranch {
        match self {
            Scope::Base => CanBranch::No,
            Scope::Class { parent, .. } => parent.can_break(),
            Scope::Nested { can_break, .. } => *can_break,
        }
    }

    fn can_continue(&'s self) -> CanBranch {
        match self {
            Scope::Base => CanBranch::No,
            Scope::Class { parent, .. } => parent.can_continue(),
            Scope::Nested { can_continue, .. } => *can_continue,
        }
    }

    fn can_return(&'s self) -> CanBranch {
        match self {
            Scope::Base => CanBranch::No,
            Scope::Class { parent, .. } => parent.can_return(),
            Scope::Nested { can_return, .. } => *can_return,
        }
    }

    fn is_top_level(&self) -> bool {
        matches!(
            self,
            Scope::Nested {
                parent: Scope::Base,
                ..
            }
        )
    }

    fn is_class(&self) -> bool {
        matches!(self, Scope::Class { .. })
    }

    fn nested(&'s self) -> Self {
        Self::Nested {
            kind: ScopeKind::Normal,
            can_break: self.can_break(),
            can_continue: self.can_continue(),
            can_return: self.can_return(),
            nl_break: Cell::new(false),
            nl_continue: Cell::new(false),
            nl_return: Cell::new(None),
            vars: ArenaVec::new(),
            parent: self,
            index: HashMap::new(),
        }
    }

    fn nested_loop(&'s self) -> Self {
        Self::Nested {
            kind: ScopeKind::Loop,
            can_break: CanBranch::Yes,
            can_continue: CanBranch::Yes,
            can_return: self.can_return(),
            nl_break: Cell::new(false),
            nl_continue: Cell::new(false),
            nl_return: Cell::new(None),
            vars: ArenaVec::new(),
            parent: self,
            index: HashMap::new(),
        }
    }

    fn function(&'s self, can_return: bool) -> Self {
        Self::Nested {
            kind: ScopeKind::Function,
            can_break: CanBranch::No,
            can_continue: CanBranch::No,
            can_return: if can_return {
                CanBranch::Yes
            } else {
                CanBranch::No
            },
            nl_break: Cell::new(false),
            nl_continue: Cell::new(false),
            nl_return: Cell::new(None),
            vars: ArenaVec::new(),
            parent: self,
            index: HashMap::new(),
        }
    }

    fn class(&'s self) -> Self {
        Self::Class {
            parent: self,
            class_private: HashMap::new(),
        }
    }

    /// Count scope depth between the current scope and the nearest enclosing
    /// loop scope. Returns 0 if no function boundary is crossed
    /// (break/continue is local).
    fn nl_break_scope_depth(&self) -> usize {
        let mut depth = 0;
        let mut last_func_depth = 0;
        let mut crossed_function = false;
        let mut scope = self;
        loop {
            match scope {
                Scope::Base => return 0,
                Scope::Class { parent, .. } => scope = parent,
                Scope::Nested { kind, parent, .. } => {
                    match kind {
                        ScopeKind::Function | ScopeKind::Lambda => {
                            crossed_function = true;
                            last_func_depth = depth;
                        }
                        ScopeKind::Loop => {
                            return if crossed_function {
                                last_func_depth + 1
                            } else {
                                0
                            };
                        }
                        ScopeKind::Normal => (),
                    }
                    depth += 1;
                    scope = parent;
                }
            }
        }
    }

    /// Count scope depth between the current scope and the nearest enclosing
    /// def scope. Returns 0 if return is local.
    fn nl_return_scope_depth(&self) -> usize {
        let mut depth: usize = 0;
        let mut crossed_lambda = false;
        let mut last_func_depth = 0;
        let mut scope = self;
        loop {
            match scope {
                Scope::Base => return 0,
                Scope::Class { parent, .. } => {
                    depth += 1;
                    scope = parent;
                }
                Scope::Nested { kind, parent, .. } => {
                    match kind {
                        ScopeKind::Function => {
                            return if crossed_lambda {
                                last_func_depth + 1
                            } else {
                                0
                            };
                        }
                        ScopeKind::Lambda => {
                            crossed_lambda = true;
                            last_func_depth = depth;
                        }
                        ScopeKind::Loop | ScopeKind::Normal => (),
                    }
                    depth += 1;
                    scope = parent;
                }
            }
        }
    }

    /// Set the NL break flag on the outermost function boundary scope
    /// before the target loop scope.
    fn mark_nl_break(&self, mut depth: usize) {
        let mut scope = self;
        loop {
            scope = match scope {
                Scope::Base => unreachable!(),
                Scope::Class { parent, .. } => {
                    if depth == 0 {
                        unreachable!();
                    }
                    parent
                }
                Scope::Nested {
                    nl_break, parent, ..
                } => {
                    if depth == 0 {
                        nl_break.set(true);
                        break;
                    }
                    parent
                }
            };
            depth -= 1;
        }
    }

    fn mark_nl_continue(&self, mut depth: usize) {
        let mut scope = self;
        loop {
            scope = match scope {
                Scope::Base => unreachable!(),
                Scope::Class { parent, .. } => {
                    if depth == 0 {
                        unreachable!();
                    }
                    parent
                }
                Scope::Nested {
                    nl_continue,
                    parent,
                    ..
                } => {
                    if depth == 0 {
                        nl_continue.set(true);
                        break;
                    }
                    parent
                }
            };
            depth -= 1;
        }
    }

    fn mark_nl_return(&self, mut depth: usize, epoch: Epoch) -> usize {
        let mut scope = self;
        loop {
            scope = match scope {
                Scope::Base => unreachable!(),
                Scope::Class { parent, .. } => {
                    if depth == 0 {
                        return parent.insert_synthetic(epoch);
                    }
                    parent
                }
                Scope::Nested {
                    nl_return, parent, ..
                } => {
                    if depth == 0 {
                        if let Some(index) = nl_return.get() {
                            return index;
                        } else {
                            let index = scope.insert_synthetic(epoch);
                            nl_return.set(Some(index));
                            return index;
                        }
                    }
                    parent
                }
            };
            depth -= 1;
        }
    }

    /// Take and clear the NL flags. Returns (has_break, has_continue, has_return).
    fn take_nl_state(&self) -> (bool, bool, Option<usize>) {
        match self {
            Scope::Base => (false, false, None),
            Scope::Class { .. } => (false, false, None),
            Scope::Nested {
                nl_break,
                nl_continue,
                nl_return,
                ..
            } => {
                let b = nl_break.replace(false);
                let c = nl_continue.replace(false);
                let r = nl_return.replace(None);
                (b, c, r)
            }
        }
    }

    fn mark_captures_since(&self, epoch: Epoch) {
        match self {
            Scope::Base => (),
            Scope::Class { parent, .. } => parent.mark_captures_since(epoch),
            Scope::Nested {
                kind, vars, parent, ..
            } => {
                for cell in vars.iter() {
                    cell.update(|(mut var, e)| {
                        // Not necessary to update epoch; being marked captured is idempotent
                        if e > epoch {
                            var.captured = true;
                        }
                        (var, e)
                    })
                }
                if !matches!(kind, ScopeKind::Function | ScopeKind::Lambda) {
                    parent.mark_captures_since(epoch)
                }
            }
        }
    }

    fn lambda(&'s self, bad_nl: Option<Span>) -> Self {
        Self::Nested {
            kind: ScopeKind::Lambda,
            can_break: self.can_break().bad_nl(bad_nl),
            can_continue: self.can_continue().bad_nl(bad_nl),
            can_return: self.can_return().bad_nl(bad_nl),
            nl_break: Cell::new(false),
            nl_continue: Cell::new(false),
            nl_return: Cell::new(None),
            vars: ArenaVec::new(),
            parent: self,
            index: HashMap::new(),
        }
    }

    fn insert_private_field(&mut self, name: String, sym: sym::Id) {
        match self {
            Scope::Class { class_private, .. } => {
                class_private.insert(name, sym);
            }
            _ => unreachable!("private field insert outside class scope"),
        }
    }

    /// Walk the scope chain to the nearest enclosing class scope and look up
    /// a private field by plain name. Returns `Some(private_sym)` if found.
    /// Always stops at the first class scope (never looks in parent classes).
    fn lookup_private_field(&self, name: &str) -> Option<sym::Id> {
        let mut scope = self;
        loop {
            match scope {
                Scope::Base => return None,
                Scope::Class { class_private, .. } => return class_private.get(name).copied(),
                Scope::Nested { parent, .. } => scope = parent,
            }
        }
    }

    /// Returns true if `name` is registered as a private field in the nearest class scope.
    fn is_private_field(&self, name: &str) -> bool {
        self.lookup_private_field(name).is_some()
    }

    fn insert(&mut self, sym: sym::Id, origin: Origin, epoch: Epoch, exported: bool) -> usize {
        self.insert_with_lookup(sym, sym, origin, epoch, exported)
    }

    fn insert_with_lookup(
        &mut self,
        lookup_sym: sym::Id,
        sym: sym::Id,
        origin: Origin,
        epoch: Epoch,
        exported: bool,
    ) -> usize {
        match self {
            Self::Base => panic!("Can't insert into base scope"),
            Self::Class { .. } => unreachable!("class scope is not lexical"),
            Self::Nested { vars, index, .. } => {
                let i = vars.len();
                vars.push(Cell::new((
                    Var {
                        sym,
                        captured: false,
                        exported,
                        used: false,
                        origin,
                        node: None,
                    },
                    epoch,
                )));
                index.insert(lookup_sym, i);
                i
            }
        }
    }

    fn insert_synthetic(&self, epoch: Epoch) -> usize {
        match self {
            Self::Base => panic!("Can't insert into base scope"),
            Self::Class { .. } => unreachable!("class scope is not lexical"),
            Self::Nested { vars, .. } => {
                let i = vars.len();
                vars.push(Cell::new((
                    Var {
                        sym: sym::Id::new(usize::MAX),
                        captured: false,
                        exported: false,
                        used: true,
                        origin: Origin::Synthetic,
                        node: None,
                    },
                    epoch,
                )));
                i
            }
        }
    }

    fn mark_local_used(&self, index: usize, epoch: Epoch) {
        match self {
            Self::Base => panic!("Can't mark vars in base scope"),
            Self::Class { .. } => unreachable!("class scope has no locals"),
            Self::Nested { vars, .. } => {
                vars[index].update(|(mut var, _)| {
                    var.used = true;
                    (var, epoch)
                });
            }
        }
    }

    fn mark_local_exported(&self, index: usize, epoch: Epoch) {
        match self {
            Self::Base => panic!("Can't mark vars in base scope"),
            Self::Class { .. } => unreachable!("class scope is not lexical"),
            Self::Nested { vars, .. } => {
                vars[index].update(|(mut var, _)| {
                    var.exported = true;
                    (var, epoch)
                });
            }
        }
    }

    fn resolve_inner(
        &self,
        id: sym::Id,
        capture: bool,
        promote: Option<Origin>,
        epoch: Epoch,
    ) -> result::Result<Res, ResolveError> {
        match self {
            Scope::Base => Err(ResolveError::Unbound),
            Scope::Class { parent, .. } => parent.resolve_inner(id, capture, promote, epoch),
            Scope::Nested {
                kind,
                parent,
                index,
                vars,
                ..
            } => {
                if let Some(&index) = index.get(&id) {
                    vars[index].update(|(var, _)| {
                        let mut var = var;
                        if capture {
                            var.captured = true;
                        }
                        var.used = true;
                        if let Some(origin) = promote {
                            var.origin = origin;
                        }
                        (var, epoch)
                    });
                    return Ok(Res {
                        index,
                        depth: 0,
                        node: None,
                    });
                }
                let Res { index, depth, .. } = parent.resolve_inner(
                    id,
                    capture || matches!(kind, ScopeKind::Function | ScopeKind::Lambda),
                    promote,
                    epoch,
                )?;
                Ok(Res {
                    index,
                    depth: depth + 1,
                    node: None,
                })
            }
        }
    }

    fn origin(&self, res: Res) -> Origin {
        match self {
            Self::Base => unreachable!(),
            Self::Class { parent, .. } => parent.origin(res),
            Self::Nested { vars, parent, .. } => {
                if res.depth == 0 {
                    vars[res.index].get().0.origin
                } else {
                    parent.origin(Res {
                        depth: res.depth - 1,
                        ..res
                    })
                }
            }
        }
    }

    fn is_read(&self, index: usize, depth: usize) -> bool {
        match self {
            Scope::Base => panic!("is_read on base scope"),
            Scope::Class { parent, .. } => parent.is_read(index, depth),
            Scope::Nested { vars, parent, .. } => {
                if depth == 0 {
                    vars[index].get().0.used
                } else {
                    parent.is_read(index, depth - 1)
                }
            }
        }
    }

    fn resolve(&self, id: sym::Id, epoch: Epoch) -> result::Result<Res, ResolveError> {
        self.resolve_inner(id, false, None, epoch)
    }

    fn promote(
        &self,
        id: sym::Id,
        origin: Origin,
        epoch: Epoch,
    ) -> result::Result<Res, ResolveError> {
        self.resolve_inner(id, false, Some(origin), epoch)
    }

    fn finish(self, resolver: &Elaborater, out: &mut Vec<Var>) {
        match self {
            Self::Nested {
                vars: ref locals, ..
            } => {
                for local in locals.iter() {
                    let (var, _) = local.get();
                    if let Some(span) = self.should_warn_unused(resolver, &var) {
                        resolver.diags.push(UnusedVar(span));
                    }
                    out.push(var);
                }
            }
            _ => panic!("Can't drain locals from non-function scope"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Error;

pub(crate) type Result<T> = result::Result<T, Error>;

enum ResolveError {
    Unbound,
}

impl<'a> Elaborater<'a> {
    // Bump epoch, returning *prior* value
    fn bump_epoch(&mut self) -> Epoch {
        let epoch = self.epoch;
        self.epoch += 1;
        epoch
    }

    fn module_name_first(module: &str) -> &str {
        if let Some((first, _)) = module.split_once(".") {
            first
        } else {
            module
        }
    }

    fn module_span_first(&self, module: Span) -> Span {
        let first = Self::module_name_first(self.file.str(module));
        Span {
            start: module.start,
            end: module.start + first.len() as u32,
        }
    }

    fn visit_ident(&mut self, scope: &mut Scope, node: &mut ast::Ident) -> Result<()> {
        let id = self
            .symtab
            .id(&self.bintab.id_str(self.file.str(node.span)));
        match scope.resolve(id, self.epoch) {
            Ok(res) => node.res = Some(res),
            Err(ResolveError::Unbound) => {
                node.res = None;
                // Handle error but leave a diagnostic and fail later
                self.fail = true;
                self.diags.push(Unbound(node.span));
            }
        }
        Ok(())
    }

    fn visit_array_elem(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut ArrayElem,
        is_arg: bool,
    ) -> Result<()> {
        match node {
            ArrayElem::Single(Single { expr, .. }) | ArrayElem::Expand(Expand { expr, .. }) => {
                self.visit_expr(scope, expr, is_arg)
            }
            ArrayElem::If(node) => self.visit_elem_if(scope, node, is_arg, Self::visit_array_elem),
            ArrayElem::For(For {
                bind,
                expr,
                body,
                iter,
                ..
            }) => {
                if let Some(expr) = expr {
                    self.visit_expr(scope, expr, false)?;
                }
                // Generate synthetic, unnameable variable to hold iterator
                let index = scope.insert_synthetic(self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    node: None,
                });
                {
                    let mut scope = scope.nested_loop();
                    // Inject loop binds into inner scope
                    match bind {
                        Pattern::Ident(ident) => self.bind_ident(&mut scope, ident, false)?,
                        Pattern::Unpack(params) => {
                            for param in params.iter_mut() {
                                self.visit_param_non_const_default(&mut scope, param)?;
                                match param {
                                    Param::Pos { ident, .. }
                                    | Param::Key { ident, .. }
                                    | Param::ConstKey { ident, .. } => {
                                        self.bind_ident(&mut scope, ident, false)?
                                    }
                                    Param::Rest { ident, .. } => {
                                        if let Some(ident) = ident {
                                            self.bind_ident(&mut scope, ident, false)?
                                        }
                                    }
                                }
                            }
                        }
                    }
                    for arg in body.elems.iter_mut() {
                        self.visit_array_elem(&mut scope, arg, is_arg)?;
                    }
                    scope.finish(self, &mut body.vars);
                }
                Ok(())
            }
        }
    }

    fn visit_dict_elem(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut DictElem,
        is_arg: bool,
    ) -> Result<()> {
        match node {
            DictElem::Single(Single { expr, .. }) => self.visit_expr(scope, expr, is_arg),
            DictElem::Key(Key { expr, .. }) => self.visit_expr(scope, expr, is_arg),
            DictElem::Pair(Pair { key, value, .. }) => {
                self.visit_expr(scope, key, is_arg)?;
                self.visit_expr(scope, value, is_arg)
            }
            DictElem::Expand(Expand { expr, .. }) => self.visit_expr(scope, expr, is_arg),
            DictElem::If(node) => self.visit_elem_if(scope, node, is_arg, Self::visit_dict_elem),
            DictElem::For(For {
                bind,
                expr,
                body,
                iter,
                ..
            }) => {
                if let Some(expr) = expr {
                    self.visit_expr(scope, expr, false)?;
                }
                // Generate synthetic, unnameable variable to hold iterator
                let index = scope.insert_synthetic(self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    node: None,
                });
                {
                    let mut scope = scope.nested_loop();
                    // Inject loop binds into inner scope
                    match bind {
                        Pattern::Ident(ident) => self.bind_ident(&mut scope, ident, false)?,
                        Pattern::Unpack(params) => {
                            for param in params.iter_mut() {
                                self.visit_param_non_const_default(&mut scope, param)?;
                                match param {
                                    Param::Pos { ident, .. }
                                    | Param::Key { ident, .. }
                                    | Param::ConstKey { ident, .. } => {
                                        self.bind_ident(&mut scope, ident, false)?
                                    }
                                    Param::Rest { ident, .. } => {
                                        if let Some(ident) = ident {
                                            self.bind_ident(&mut scope, ident, false)?
                                        }
                                    }
                                }
                            }
                        }
                    }
                    for elem in body.elems.iter_mut() {
                        self.visit_dict_elem(&mut scope, elem, is_arg)?;
                    }
                    scope.finish(self, &mut body.vars);
                }
                Ok(())
            }
        }
    }

    fn is_binary_operator_literal(literal_content: &str) -> bool {
        matches!(
            literal_content,
            "||" | "&&"
                | "|"
                | "^"
                | "&"
                | "<"
                | "<="
                | ">"
                | ">="
                | "=="
                | "!="
                | "+"
                | "-"
                | "*"
                | "/"
                | "//"
                | "%"
                | "<<"
                | ">>"
        )
    }

    fn visit_expr(&mut self, scope: &mut Scope<'_>, node: &mut Expr, is_arg: bool) -> Result<()> {
        match node {
            Expr::Ident(ident) => self.visit_ident(scope, ident),
            Expr::Group { expr, .. } => self.visit_expr(scope, expr, is_arg),
            Expr::Unary { expr, .. } => self.visit_expr(scope, expr, is_arg),
            Expr::Binary { exprs, .. } => {
                self.visit_expr(scope, &mut exprs[0], is_arg)?;
                self.visit_expr(scope, &mut exprs[1], is_arg)
            }
            Expr::Range { exprs, .. } => {
                if let Some(start) = &mut exprs[0] {
                    self.visit_expr(scope, start, is_arg)?;
                }
                if let Some(end) = &mut exprs[1] {
                    self.visit_expr(scope, end, is_arg)?;
                }
                Ok(())
            }
            Expr::Lambda { func, do_span, .. } => {
                let span = do_span.unwrap_or_else(|| func.span());
                self.visit_lambda(scope, func, if is_arg { None } else { Some(span) })
            }
            Expr::Call { arg0, args, .. } => {
                self.visit_expr(scope, arg0, is_arg)?;

                for arg in args.iter_mut() {
                    self.visit_cmd_arg(scope, arg)?;
                }

                if let Some(Arg::Pos(Single {
                    expr: Expr::Literal(span),
                    ..
                })) = args.first()
                    && args.len() >= 2
                {
                    let span = *span;
                    let content = self.file.str(span);
                    if Self::is_binary_operator_literal(content) {
                        self.diags.push(BinaryOpAsArg {
                            span: node.span(),
                            operator_span: span,
                        });
                    }
                } else if let Expr::Literal(_)
                | Expr::Int(_, _)
                | Expr::VerbatimInt(_, _)
                | Expr::F64(_, _)
                | Expr::VerbatimF64(_, _)
                | Expr::Bool(_, _)
                | Expr::Nil(_)
                | Expr::Sym(_)
                | Expr::Array { .. }
                | Expr::Dict { .. }
                | Expr::Concat { .. }
                | Expr::FmtSeq { .. }
                | Expr::FmtParam { .. }
                | Expr::BinConcat { .. } = &**arg0
                {
                    self.diags.push(Uncallable {
                        expr_span: arg0.span(),
                        span: node.span(),
                    });
                }
                Ok(())
            }
            Expr::Get { object, field, .. } => {
                self.visit_expr(scope, object, is_arg)?;
                match field {
                    GetVariant::Private { span, res } => {
                        let name = self.file.str(*span);
                        if let Some(private_sym) = scope.lookup_private_field(name) {
                            *res = Some(private_sym);
                        } else {
                            self.diags.push(NoPrivateField {
                                span: *span,
                                name: name.to_owned(),
                            });
                            self.fail = true;
                        }
                    }
                    GetVariant::Normal(span) => {
                        // Warn if this looks like accessing a private field on `self`
                        // without using the `.#field` syntax
                        let name = self.file.str(*span);
                        if scope.is_private_field(name)
                            && let Expr::Ident(ident) = object.as_ref()
                            && let Some(res) = ident.res
                            && matches!(scope.origin(res), Origin::SelfParam(_))
                        {
                            self.diags.push(PrivateFieldWithoutHash { span: *span });
                        }
                    }
                    GetVariant::SpecialMethod { .. } => {}
                }
                Ok(())
            }
            Expr::Index { exprs, .. } => {
                self.visit_expr(scope, &mut exprs[0], is_arg)?;
                self.visit_expr(scope, &mut exprs[1], is_arg)?;
                Ok(())
            }
            Expr::Array { elems, .. } => {
                for elem in elems.iter_mut() {
                    self.visit_array_elem(scope, elem, is_arg)?;
                }
                Ok(())
            }
            Expr::Dict { elems, .. } => {
                for elem in elems.iter_mut() {
                    self.visit_dict_elem(scope, elem, is_arg)?;
                }
                Ok(())
            }
            Expr::Concat { exprs, .. } => {
                for expr in exprs.iter_mut() {
                    self.visit_expr(scope, expr, is_arg)?;
                }
                Ok(())
            }
            Expr::Fmt { value, spec, .. } => {
                self.visit_expr(scope, value, is_arg)?;
                for expr in [&mut spec.width, &mut spec.precision].into_iter().flatten() {
                    self.visit_expr(scope, expr, is_arg)?;
                }
                Ok(())
            }
            // A parameter's name is a symbol, not a reference to anything in
            // scope, so only the dynamic counts are resolved.
            Expr::FmtParam { spec, .. } => {
                for expr in [&mut spec.width, &mut spec.precision].into_iter().flatten() {
                    self.visit_expr(scope, expr, is_arg)?;
                }
                Ok(())
            }
            Expr::FmtSeq { exprs, .. } | Expr::BinConcat { exprs, .. } => {
                for expr in exprs.iter_mut() {
                    self.visit_expr(scope, expr, is_arg)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn visit_lvalue(&mut self, scope: &mut Scope, node: &mut LValue) -> Result<()> {
        match node {
            LValue::Ident(id) => self.visit_ident(scope, id),
            LValue::Field { object, field, .. } => {
                self.visit_expr(scope, object, false)?;
                // Warn if this looks like accessing a private field on `self`
                // without using the `.#field` syntax
                let name = self.file.str(*field);
                if scope.is_private_field(name)
                    && let Expr::Ident(ident) = object.as_ref()
                    && let Some(res) = ident.res
                    && matches!(scope.origin(res), Origin::SelfParam(_))
                {
                    self.diags.push(PrivateFieldWithoutHash { span: *field });
                }
                Ok(())
            }
            LValue::PrivateField {
                object, field, res, ..
            } => {
                self.visit_expr(scope, object, false)?;
                let name = self.file.str(*field);
                if let Some(private_sym) = scope.lookup_private_field(name) {
                    *res = Some(private_sym);
                } else {
                    self.diags.push(NoPrivateField {
                        span: *field,
                        name: name.to_owned(),
                    });
                    self.fail = true;
                }
                Ok(())
            }
            LValue::Index { exprs, .. } => {
                self.visit_expr(scope, &mut exprs[0], false)?;
                self.visit_expr(scope, &mut exprs[1], false)
            }
        }
    }

    fn visit_cmd_arg(&mut self, scope: &mut Scope<'_>, node: &mut Arg) -> Result<()> {
        match node {
            Arg::Pos(Single { expr, .. }) => self.visit_expr(scope, expr, true),
            Arg::Key(Key { expr, .. }) => self.visit_expr(scope, expr, true),
            Arg::DynamicKey(Pair { key, value, .. }) => {
                self.visit_expr(scope, key, true)?;
                self.visit_expr(scope, value, true)
            }
            Arg::Expand(Expand { expr, .. }) => self.visit_expr(scope, expr, true),
            Arg::If(node) => self.visit_elem_if(scope, node, false, |this, scope, elem, _| {
                this.visit_cmd_arg(scope, elem)
            }),
            Arg::For(For {
                bind,
                expr,
                body,
                iter,
                ..
            }) => {
                if let Some(expr) = expr {
                    self.visit_expr(scope, expr, false)?;
                }
                // Generate synthetic, unnameable variable to hold iterator
                let index = scope.insert_synthetic(self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    node: None,
                });
                {
                    let mut scope = scope.nested_loop();
                    // Inject loop binds into inner scope
                    match bind {
                        Pattern::Ident(ident) => self.bind_ident(&mut scope, ident, false)?,
                        Pattern::Unpack(params) => {
                            for param in params.iter_mut() {
                                self.visit_param_non_const_default(&mut scope, param)?;
                                match param {
                                    Param::Pos { ident, .. }
                                    | Param::Key { ident, .. }
                                    | Param::ConstKey { ident, .. } => {
                                        self.bind_ident(&mut scope, ident, false)?
                                    }
                                    Param::Rest { ident, .. } => {
                                        if let Some(ident) = ident {
                                            self.bind_ident(&mut scope, ident, false)?
                                        }
                                    }
                                }
                            }
                        }
                    }
                    for arg in body.elems.iter_mut() {
                        self.visit_cmd_arg(&mut scope, arg)?;
                    }
                    scope.finish(self, &mut body.vars);
                }
                Ok(())
            }
        }
    }

    fn bind_ident(&mut self, scope: &mut Scope<'_>, ident: &mut Ident, export: bool) -> Result<()> {
        let id = self
            .symtab
            .id(&self.bintab.id_str(self.file.str(ident.span)));
        let node = Origin::Source(ident.span);
        let index = scope.insert(id, node, self.epoch, export);
        ident.res = Some(Res {
            index,
            depth: 0,
            node: None,
        });
        Ok(())
    }

    fn visit_let(&mut self, scope: &mut Scope<'_>, node: &mut Let) -> Result<()> {
        if let Some(span) = node.pub_span
            && !scope.is_top_level()
            && !scope.is_class()
        {
            self.diags.push(InappropriatePub(span));
            self.fail = true;
        }

        // Check for `if` without `else` in RHS
        if let PrimStmt::If(if_node) = &node.rhs
            && if_node.else_branch.is_none()
        {
            self.diags.push(IfWithoutElse(if_node.tbranch.span));
        }

        self.visit_prim_stmt(scope, &mut node.rhs, true)?;

        // In a class body, let bindings are not inserted into the lexical index.
        // Private fields use their unique private sym; pub fields use the plain sym.
        if scope.is_class()
            && let Pattern::Ident(ident) = &mut node.bind
        {
            let name = self.file.str(ident.span);
            let sym = if node.pub_span.is_none() {
                scope
                    .lookup_private_field(name)
                    .expect("private sym should exist from pre-scan")
            } else {
                self.symtab.id(&self.bintab.id_str(name))
            };
            let origin = Origin::Source(ident.span);
            let lookup_sym = self.symtab.id(&self.bintab.id_str(name));
            let index = scope.insert_with_lookup(lookup_sym, sym, origin, self.epoch, true);
            ident.res = Some(Res {
                index,
                depth: 0,
                node: None,
            });
            return Ok(());
        }

        self.visit_pattern(scope, &mut node.bind, node.pub_span.is_some())?;
        Ok(())
    }

    fn visit_bind(&mut self, scope: &mut Scope<'_>, node: &mut Bind) -> Result<()> {
        self.visit_expr(scope, &mut node.expr, false)?;
        self.visit_pattern(scope, &mut node.bind, false)?;
        Ok(())
    }

    fn visit_param_non_const_default(
        &mut self,
        scope: &mut Scope<'_>,
        param: &mut Param,
    ) -> Result<()> {
        let default = match param {
            Param::Pos { default, .. }
            | Param::Key { default, .. }
            | Param::ConstKey { default, .. } => default,
            Param::Rest { .. } => return Ok(()),
        };
        if let Some(default) = default
            && default.fold.is_none()
        {
            self.visit_expr(scope, &mut default.expr, false)?;
        }
        Ok(())
    }

    fn visit_pattern(
        &mut self,
        scope: &mut Scope<'_>,
        pat: &mut Pattern,
        export: bool,
    ) -> Result<()> {
        match pat {
            Pattern::Ident(ident) => self.bind_ident(scope, ident, export)?,
            Pattern::Unpack(params) => {
                for param in params.iter_mut() {
                    self.visit_param_non_const_default(scope, param)?;
                    match param {
                        Param::Pos { ident, .. }
                        | Param::Key { ident, .. }
                        | Param::ConstKey { ident, .. } => self.bind_ident(scope, ident, export)?,
                        Param::Rest { ident, .. } => {
                            if let Some(ident) = ident {
                                self.bind_ident(scope, ident, export)?
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn visit_assign(&mut self, scope: &mut Scope<'_>, node: &mut Assign) -> Result<()> {
        self.visit_lvalue(scope, &mut node.lhs)?;

        // Check for `if` without `else` in RHS
        if let PrimStmt::If(if_node) = &node.rhs
            && if_node.else_branch.is_none()
        {
            self.diags.push(IfWithoutElse(if_node.tbranch.span));
        }

        self.visit_prim_stmt(scope, &mut node.rhs, true)
    }

    /// Inject the bindings of a destructuring pattern into `scope`.
    ///
    /// The bindings land at the front of the scope, which lowering relies on to
    /// resolve them positionally.
    fn bind_pattern(&mut self, scope: &mut Scope<'_>, pattern: &mut Pattern) -> Result<()> {
        match pattern {
            Pattern::Ident(ident) => self.bind_ident(scope, ident, false)?,
            Pattern::Unpack(params) => {
                for param in params.iter_mut() {
                    self.visit_param_non_const_default(scope, param)?;
                    match param {
                        Param::Pos { ident, .. }
                        | Param::Key { ident, .. }
                        | Param::ConstKey { ident, .. } => self.bind_ident(scope, ident, false)?,
                        Param::Rest { ident, .. } => {
                            if let Some(ident) = ident {
                                self.bind_ident(scope, ident, false)?
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Visit the body of an `if` or `while` branch, injecting the bindings of its
    /// conditional pattern, if any, before the body's own statements.
    ///
    /// The condition itself belongs to the enclosing scope and must already have
    /// been visited there: only the branch body can see the bindings.
    fn visit_branch_body(
        &mut self,
        scope: &mut Scope<'_>,
        bind: Option<&mut PatternBind>,
        body: &mut Block,
        is_loop: bool,
    ) -> Result<()> {
        let mut inner = if is_loop {
            scope.nested_loop()
        } else {
            scope.nested()
        };
        if let Some(bind) = bind {
            self.bind_pattern(&mut inner, &mut bind.pattern)?;
        }
        self.visit_block_inner(&mut inner, body)?;
        inner.finish(self, &mut body.vars);
        Ok(())
    }

    /// Elaborate an `if` in vertical-element layout, where each branch body is a
    /// list of arguments, array elements, or dict elements rather than a block.
    ///
    /// Structurally identical to [`Self::visit_if`]: each branch body gets a scope
    /// of its own so that a conditional pattern has somewhere to bind.  `visit_elem`
    /// is the caller's per-element visitor, and `is_arg` is threaded through to it.
    fn visit_elem_if<T>(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut If<ExprBody<T>>,
        is_arg: bool,
        visit_elem: fn(&mut Self, &mut Scope<'_>, &mut T, bool) -> Result<()>,
    ) -> Result<()> {
        self.visit_expr(scope, &mut node.tbranch.expr, false)?;

        self.visit_elem_branch_body(
            scope,
            node.tbranch.bind.as_mut(),
            &mut node.tbranch.body,
            is_arg,
            visit_elem,
        )?;

        for (elif_branch, _) in &mut node.elif_branches {
            self.visit_expr(scope, &mut elif_branch.expr, false)?;

            self.visit_elem_branch_body(
                scope,
                elif_branch.bind.as_mut(),
                &mut elif_branch.body,
                is_arg,
                visit_elem,
            )?;
        }

        if let Some((else_body, _)) = &mut node.else_branch {
            self.visit_elem_branch_body(scope, None, else_body, is_arg, visit_elem)?;
        }

        Ok(())
    }

    /// The vertical-element counterpart of [`Self::visit_branch_body`].
    fn visit_elem_branch_body<T>(
        &mut self,
        scope: &mut Scope<'_>,
        bind: Option<&mut PatternBind>,
        body: &mut ExprBody<T>,
        is_arg: bool,
        visit_elem: fn(&mut Self, &mut Scope<'_>, &mut T, bool) -> Result<()>,
    ) -> Result<()> {
        let mut inner = scope.nested();
        if let Some(bind) = bind {
            self.bind_pattern(&mut inner, &mut bind.pattern)?;
        }
        for elem in body.elems.iter_mut() {
            visit_elem(self, &mut inner, elem, is_arg)?;
        }
        inner.finish(self, &mut body.vars);
        Ok(())
    }

    fn visit_while(&mut self, scope: &mut Scope<'_>, node: &mut While) -> Result<()> {
        self.visit_expr(scope, &mut node.expr, false)?;
        self.visit_branch_body(scope, node.bind.as_mut(), &mut node.body, true)
    }

    fn visit_if(&mut self, scope: &mut Scope<'_>, node: &mut If<Block>) -> Result<()> {
        // Visit first if branch
        self.visit_expr(scope, &mut node.tbranch.expr, false)?;
        self.visit_branch_body(
            scope,
            node.tbranch.bind.as_mut(),
            &mut node.tbranch.body,
            false,
        )?;

        // Visit elif branches
        for (elif_branch, _) in &mut node.elif_branches {
            self.visit_expr(scope, &mut elif_branch.expr, false)?;
            self.visit_branch_body(
                scope,
                elif_branch.bind.as_mut(),
                &mut elif_branch.body,
                false,
            )?;
        }

        // Visit final else branch if present
        if let Some((else_block, _)) = &mut node.else_branch {
            self.visit_block(scope, else_block)?;
        }

        Ok(())
    }

    /// Elaborate try, catch, and finally bodies as closures.
    fn visit_try(&mut self, scope: &mut Scope<'_>, node: &mut Try) -> Result<()> {
        // Visit body as a function scope (0-param closure)
        self.visit_lambda(scope, &mut node.body, None)?;

        // For each handler: visit class_expr in outer scope, then handler func as function scope
        for handler in &mut node.handlers {
            if let Some(class_expr) = &mut handler.class_expr {
                self.visit_expr(scope, class_expr, false)?;
            }
            self.visit_lambda(scope, &mut handler.func, None)?;
        }

        // Visit finally as function scope if present
        if let Some((finally_func, _)) = &mut node.finally {
            self.visit_lambda(scope, finally_func, None)?;
        }

        Ok(())
    }

    fn visit_import_pre(&mut self, scope: &mut Scope<'_>, import: &mut Import) -> Result<()> {
        let exported = import.pub_span.is_some();
        if let Some(span) = import.pub_span
            && !scope.is_top_level()
        {
            self.diags.push(InappropriatePub(span));
            self.fail = true;
        }
        for element in &mut import.elements {
            match element {
                ImportElement::ModuleAsIs {
                    module,
                    bind,
                    insert,
                } => {
                    let id = self
                        .symtab
                        .id(&self.bintab.id_str(self.file.str(bind.span)));
                    let name = self.module_span_first(*module);
                    let node = Origin::Source(name);
                    if let Ok(res) = scope.promote(id, node, self.epoch)
                        && res.depth == 0
                    {
                        // Reuse the binding and update its source provenance.
                        *insert = true;
                        bind.res = Some(res);
                        if exported {
                            scope.mark_local_exported(res.index, self.epoch);
                        }
                    } else {
                        let index = scope.insert(id, node, self.epoch, exported);
                        bind.res = Some(Res {
                            index,
                            depth: 0,
                            node: None,
                        });
                    }
                }
                ImportElement::ModuleRenamed { bind, .. } => {
                    let id = self
                        .symtab
                        .id(&self.bintab.id_str(self.file.str(bind.span)));
                    let node = Origin::Source(bind.span);
                    let index = scope.insert(id, node, self.epoch, exported);
                    bind.res = Some(Res {
                        index,
                        depth: 0,
                        node: None,
                    });
                }
                ImportElement::Items { items, .. } => {
                    assert!(!items.is_empty());
                    for item in items.iter_mut() {
                        let bind = match item {
                            ImportItem::AsIs { bind, .. } | ImportItem::Renamed { bind, .. } => {
                                bind
                            }
                        };
                        let id = self
                            .symtab
                            .id(&self.bintab.id_str(self.file.str(bind.span)));
                        let node = Origin::Source(bind.span);
                        let index = scope.insert(id, node, self.epoch, exported);
                        bind.res = Some(Res {
                            index,
                            depth: 0,
                            node: None,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn visit_import(&mut self, _scope: &mut Scope<'_>, _import: &mut Import) -> Result<()> {
        // Everything was actually done in _pre
        Ok(())
    }

    fn visit_for(&mut self, scope: &mut Scope<'_>, node: &mut For<Block>) -> Result<()> {
        if let Some(expr) = &mut node.expr {
            self.visit_expr(scope, expr, false)?;
        }
        // Generate synthetic, unnameable variable to hold iterator
        let index = scope.insert_synthetic(self.epoch);
        node.iter = Some(Res {
            index,
            depth: 0,
            node: None,
        });
        {
            let mut scope = scope.nested_loop();
            // Inject loop binds into inner scope
            self.bind_pattern(&mut scope, &mut node.bind)?;
            self.visit_block_inner(&mut scope, &mut node.body)?;
            scope.finish(self, &mut node.body.vars);
        }
        Ok(())
    }

    fn visit_prim_stmt(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut PrimStmt,
        is_final: bool,
    ) -> Result<()> {
        match node {
            PrimStmt::Expr(cmd) => {
                // Check for side effects and emit warnings, but not in final position
                if !is_final {
                    match cmd.side_effect() {
                        SideEffect::None => {
                            self.diags.push(NoEffect { span: cmd.span() });
                        }
                        SideEffect::VarRef => {
                            self.diags.push(NoEffectVar {
                                span: cmd.span(),
                                expr_span: cmd.span(),
                            });
                        }
                        SideEffect::Unlikely => {
                            self.diags.push(NoApparentEffect {
                                span: cmd.span(),
                                expr_span: cmd.span(),
                            });
                        }
                        SideEffect::Likely => {}
                        SideEffect::Discarded => {
                            self.diags.push(DiscardedComputation {
                                span: cmd.span(),
                                expr_span: cmd.span(),
                            });
                        }
                    }
                }
                // Visit the expression first (this resolves variable references)
                self.visit_expr(scope, cmd, false)
            }
            PrimStmt::If(node) => self.visit_if(scope, node),
            PrimStmt::Try(node) => self.visit_try(scope, node),
        }
    }

    fn visit_stmt(&mut self, scope: &mut Scope<'_>, node: &mut Stmt, is_final: bool) -> Result<()> {
        match node {
            Stmt::Assign(node) => self.visit_assign(scope, node),
            Stmt::Bind(node) => self.visit_bind(scope, node),
            Stmt::Break(span, nl) => self.visit_break(scope, *span, nl),
            Stmt::Class(class) => self.visit_class(scope, class),
            Stmt::Continue(span, nl) => self.visit_continue(scope, *span, nl),
            Stmt::Def(def) => self.visit_def(scope, def),
            Stmt::For(node) => self.visit_for(scope, node),
            Stmt::Import(import) => self.visit_import(scope, import),
            Stmt::Let(node) => self.visit_let(scope, node),
            Stmt::NlGuard(_) => unreachable!("NlGuard should not exist before resolve"),
            Stmt::Prim(prim) => self.visit_prim_stmt(scope, prim, is_final),
            Stmt::Return(ret) => self.visit_return(scope, ret),
            Stmt::Throw(node) => self.visit_expr(scope, &mut node.expr, false),
            Stmt::While(node) => self.visit_while(scope, node),
        }
    }

    fn visit_break(
        &mut self,
        scope: &mut Scope<'_>,
        span: Span,
        nl: &mut Option<NlInfo>,
    ) -> Result<()> {
        match scope.can_break() {
            CanBranch::No => {
                self.fail = true;
                self.diags.push(BadBreak(span));
            }
            CanBranch::BadNl(lambda) => {
                self.fail = true;
                self.diags.push(BadNl {
                    span,
                    lambda_span: lambda,
                });
            }
            CanBranch::Yes => {
                let depth = scope.nl_break_scope_depth();
                if depth > 0 {
                    *nl = Some(NlInfo {
                        scope_depth: depth,
                        indicator: 1,
                        ret_upvar: None,
                    });
                    scope.mark_nl_break(depth);
                }
            }
        }
        Ok(())
    }

    fn visit_continue(
        &mut self,
        scope: &mut Scope<'_>,
        span: Span,
        nl: &mut Option<NlInfo>,
    ) -> Result<()> {
        match scope.can_continue() {
            CanBranch::No => {
                self.fail = true;
                self.diags.push(BadContinue(span));
            }
            CanBranch::BadNl(lambda) => {
                self.fail = true;
                self.diags.push(BadNl {
                    span,
                    lambda_span: lambda,
                });
            }
            CanBranch::Yes => {
                let depth = scope.nl_break_scope_depth();
                if depth > 0 {
                    *nl = Some(NlInfo {
                        scope_depth: depth,
                        indicator: 2,
                        ret_upvar: None,
                    });
                    scope.mark_nl_continue(depth);
                }
            }
        }
        Ok(())
    }

    fn visit_return(
        &mut self,
        scope: &mut Scope<'_>,
        Return { expr, span, nl }: &mut Return,
    ) -> Result<()> {
        match scope.can_return() {
            CanBranch::No => {
                self.fail = true;
                self.diags.push(BadReturn(*span));
            }
            CanBranch::BadNl(lambda) => {
                self.fail = true;
                self.diags.push(BadNl {
                    span: *span,
                    lambda_span: lambda,
                });
            }
            CanBranch::Yes => {
                let depth = scope.nl_return_scope_depth();
                if depth > 0 {
                    let index = scope.mark_nl_return(depth, self.epoch);
                    *nl = Some(NlInfo {
                        scope_depth: depth,
                        indicator: 3,
                        ret_upvar: Some(Res {
                            index,
                            depth,
                            node: None,
                        }),
                    });
                }
            }
        }
        if let Some(expr) = expr {
            self.visit_expr(scope, expr, false)?;
        }
        Ok(())
    }

    fn insert_class_method(&mut self, scope: &mut Scope<'_>, node: &mut Method) {
        node.private_sym = if node.pub_span.is_none() && node.special.is_none() {
            Some(
                scope
                    .lookup_private_field(self.file.str(node.name_span))
                    .expect("private sym should exist from pre-scan"),
            )
        } else {
            None
        };
    }

    fn insert_class_class(&mut self, scope: &mut Scope<'_>, node: &mut Class) {
        let sym = self
            .symtab
            .id(&self.bintab.id_str(self.file.str(node.ident.span)));
        let origin = Origin::Source(node.ident.span);
        let index = scope.insert(sym, origin, self.epoch, true);
        node.ident.res = Some(Res {
            index,
            depth: 0,
            node: None,
        });
    }

    fn visit_def(&mut self, scope: &mut Scope<'_>, def: &mut Def) -> Result<()> {
        // Check pub validity
        if let Some(span) = def.pub_span
            && !scope.is_top_level()
            && !scope.is_class()
        {
            self.diags.push(InappropriatePub(span));
            self.fail = true;
        }
        for decorator in &mut def.decorators {
            self.visit_expr(scope, &mut decorator.expr, false)?;
        }
        if !def.decorators.is_empty() {
            let res = def
                .ident
                .res
                .as_ref()
                .expect("decorated def should have an assigned binding");
            scope.mark_local_used(res.index, self.epoch);
        }
        self.visit_function(scope, &mut def.func, None)
    }

    fn visit_method(&mut self, scope: &mut Scope<'_>, def: &mut Method) -> Result<()> {
        for decorator in &mut def.decorators {
            self.visit_expr(scope, &mut decorator.expr, false)?;
        }
        self.visit_function(scope, &mut def.func, None)
    }

    /// Resolve a field decorator to a member-scope annotation.
    ///
    /// Fields have no runtime decorator semantics, so `class` and `static` are
    /// consumed here instead. Requiring the canonical prelude binding — rather
    /// than any expression that happens to be spelled `class` — keeps the door
    /// open for giving field decorators real semantics later.
    fn field_member_scope(
        &mut self,
        scope: &Scope<'_>,
        decorator: &ast::Decorator,
    ) -> Option<ast::MemberScope> {
        let ast::Expr::Ident(ident) = &decorator.expr else {
            return None;
        };
        let res = ident.res?;
        let Origin::PreludeItem { module, item } = scope.origin(res) else {
            return None;
        };
        if &self.bintab[module] != "std" {
            return None;
        }
        match &self.bintab[item] {
            "class" => Some(ast::MemberScope::Class),
            "static" => Some(ast::MemberScope::Static),
            _ => None,
        }
    }

    fn visit_field_decorators(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut ast::FieldDecl,
    ) -> Result<()> {
        let mut decorators = std::mem::take(&mut node.decorators);
        for decorator in &mut decorators {
            self.visit_expr(scope, &mut decorator.expr, false)?;
        }
        for decorator in &decorators {
            // Anchor on the field rather than the decorator: a comment may not sit
            // between a decorator and the declaration it applies to, and the field
            // is what the diagnostic is really about.
            let span = node.field_span;
            match self.field_member_scope(scope, decorator) {
                Some(scope) if node.scope == ast::MemberScope::Instance => node.scope = scope,
                Some(_) => self.diags.push(DuplicateMemberScope(span)),
                None => self.diags.push(UnsupportedFieldDecorator(span)),
            }
        }
        node.decorators = decorators;
        Ok(())
    }

    fn visit_field_decl(&mut self, scope: &mut Scope<'_>, node: &mut ast::FieldDecl) -> Result<()> {
        self.visit_field_decorators(scope, node)?;

        // A static field is evaluated once at class creation, so it needs no
        // thunk. Unwrap the one the parser built before resolving anything in it,
        // so the initializer resolves in the enclosing scope rather than a
        // function scope of its own.
        if node.scope == ast::MemberScope::Static
            && let ast::FieldInit::Thunk(func) = &mut node.init
            && let Some(ast::Stmt::Prim(ast::PrimStmt::Expr(expr))) = func.body.stmts.pop()
        {
            node.init = ast::FieldInit::Expr(expr);
        }

        match &mut node.init {
            ast::FieldInit::None => {}
            ast::FieldInit::Const(expr, _) | ast::FieldInit::Expr(expr) => {
                self.visit_expr(scope, expr, false)?
            }
            ast::FieldInit::Thunk(func) => self.visit_function(scope, func, None)?,
        }
        assert!(scope.is_class(), "class field outside class scope");
        for field in &mut node.fields {
            let name = self.file.str(field.ident.span);
            field.private_sym = if node.pub_span.is_none() {
                Some(
                    scope
                        .lookup_private_field(name)
                        .expect("private sym should exist from pre-scan"),
                )
            } else {
                None
            };
        }
        Ok(())
    }

    fn visit_body_pre(&mut self, scope: &mut Scope<'_>, block: &mut Block) -> Result<()> {
        for stmt in block.stmts.iter_mut() {
            match stmt {
                Stmt::Def(node) => {
                    let ident_span = node.ident.span;
                    let sym = self
                        .symtab
                        .id(&self.bintab.id_str(self.file.str(ident_span)));
                    let exported = node.pub_span.is_some();
                    let origin = Origin::Source(ident_span);
                    let index = scope.insert(sym, origin, self.epoch, exported);
                    node.ident.res = Some(Res {
                        index,
                        depth: 0,
                        node: None,
                    });
                }
                Stmt::Class(node) => {
                    let sym = self
                        .symtab
                        .id(&self.bintab.id_str(self.file.str(node.ident.span)));
                    let exported = node.pub_span.is_some();
                    let origin = Origin::Source(node.ident.span);
                    let index = scope.insert(sym, origin, self.epoch, exported);
                    node.ident.res = Some(Res {
                        index,
                        depth: 0,
                        node: None,
                    });
                }
                Stmt::Import(import) => self.visit_import_pre(scope, import)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn visit_class_body_pre(
        &mut self,
        scope: &mut Scope<'_>,
        body: &mut ast::ClassBody,
    ) -> Result<()> {
        for member in body.members.iter_mut() {
            match member {
                ast::ClassMember::Field(node) if node.pub_span.is_none() => {
                    for field in &node.fields {
                        let name = self.file.str(field.ident.span).to_owned();
                        let private_sym = self.symtab.fresh(self.bintab.id_str(&name));
                        scope.insert_private_field(name, private_sym);
                    }
                }
                ast::ClassMember::Method(node)
                    if node.pub_span.is_none() && node.special.is_none() =>
                {
                    let name = self.file.str(node.name_span).to_owned();
                    let private_sym = self.symtab.fresh(self.bintab.id_str(&name));
                    scope.insert_private_field(name, private_sym);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn visit_class_body(&mut self, scope: &mut Scope<'_>, body: &mut ast::ClassBody) -> Result<()> {
        self.visit_class_body_pre(scope, body)?;
        for member in body.members.iter_mut() {
            self.bump_epoch();
            match member {
                ast::ClassMember::Field(field) => self.visit_field_decl(scope, field)?,
                ast::ClassMember::Method(def) => {
                    self.insert_class_method(scope, def);
                    self.visit_method(scope, def)?;
                }
            }
        }
        Ok(())
    }

    fn visit_class(&mut self, scope: &mut Scope<'_>, class: &mut Class) -> Result<()> {
        // Check pub validity
        if let Some(span) = class.pub_span
            && !scope.is_top_level()
            && !scope.is_class()
        {
            self.diags.push(InappropriatePub(span));
            self.fail = true;
        }

        for decorator in &mut class.decorators {
            self.visit_expr(scope, &mut decorator.expr, false)?;
        }

        // Resolve superclass references BEFORE inserting the class name
        // (the class name should not be available in its own superclass references)
        for super_ref in &mut class.super_refs {
            self.visit_ident(scope, &mut super_ref.ident)?;
        }

        if scope.is_class() {
            self.insert_class_class(scope, class);
        }

        assert!(
            class.ident.res.is_some(),
            "class should already be registered during block pre-pass"
        );

        if !class.decorators.is_empty() {
            let res = class.ident.res.as_ref().unwrap();
            scope.mark_local_used(res.index, self.epoch);
        }

        // Visit the class body in a new class scope
        {
            let mut class_scope = scope.class();

            self.visit_class_body(&mut class_scope, &mut class.body)?;
        }

        Ok(())
    }

    fn visit_block_inner(&mut self, scope: &mut Scope<'_>, node: &mut Block) -> Result<()> {
        self.visit_body_pre(scope, node)?;
        let mut unreach = false;
        let stmt_count = node.stmts.len();
        for (idx, stmt) in node.iter_mut().enumerate() {
            let epoch = self.bump_epoch();
            let is_final = idx == stmt_count - 1;
            self.visit_stmt(scope, stmt, is_final)?;
            // Check if any NL flags were set during the visit
            let (has_break, has_continue, has_return) = scope.take_nl_state();
            if has_break || has_continue || has_return.is_some() {
                scope.mark_captures_since(epoch);
                let span = stmt.span();
                let inner = std::mem::replace(stmt, Stmt::Break(span, None));
                *stmt = Stmt::NlGuard(NlGuard {
                    body: Box::new(inner),
                    span,
                    has_break,
                    has_continue,
                    has_return: has_return.map(|index| Res {
                        index,
                        depth: 0,
                        node: None,
                    }),
                });
            }
            if unreach {
                self.diags.push(Unreachable(stmt.span()));
                unreach = false;
            }
            if matches!(
                stmt,
                Stmt::Return(..) | Stmt::Throw(..) | Stmt::Continue(..) | Stmt::Break(..)
            ) {
                unreach = true;
            }
        }
        Ok(())
    }

    /// Elaborate a block that is a construct in its own right, such as an
    /// `else` body.
    fn visit_block(&mut self, scope: &mut Scope<'_>, node: &mut Block) -> Result<()> {
        let mut scope = scope.nested();
        self.visit_block_inner(&mut scope, node)?;
        scope.finish(self, &mut node.vars);
        Ok(())
    }

    fn visit_function(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut Function,
        mut prelude: Option<&mut [PreludeImport]>,
    ) -> Result<()> {
        let is_class_method = scope.is_class();
        let mut scope = scope.function(self.mode != Mode::Repl || prelude.is_none());
        // Register all parameters as variables in this scope.
        // Visit non-constant default expressions before inserting each param,
        // so defaults can reference prior params but not the current or later ones.
        for (param_idx, param) in node.params.iter_mut().enumerate() {
            self.visit_param_non_const_default(&mut scope, param)?;
            let ident = match param {
                Param::Pos { ident, .. }
                | Param::Key { ident, .. }
                | Param::ConstKey { ident, .. } => Some(ident),
                Param::Rest { ident, .. } => ident.as_mut(),
            };
            let Some(ident) = ident else {
                continue;
            };
            let sym = self
                .symtab
                .id(&self.bintab.id_str(self.file.str(ident.span)));
            let index = scope.insert(
                sym,
                if is_class_method && param_idx == 0 {
                    Origin::SelfParam(ident.span)
                } else {
                    Origin::Source(ident.span)
                },
                self.epoch,
                false,
            );
            ident.res = Some(Res {
                index,
                depth: 0,
                node: None,
            });
        }

        if let Some(prelude) = &mut prelude {
            for import in prelude.iter_mut() {
                match import {
                    PreludeImport::Items { module, items, .. } => {
                        for field in items.iter_mut() {
                            let id = self.symtab.id(&self.bintab.id_str(&field.bind));
                            let origin = Origin::PreludeItem {
                                module: self.bintab.id_str(module),
                                item: self.bintab.id_str(&field.item),
                            };
                            let index = scope.insert(id, origin, self.epoch, false);
                            field.res = Some(Res {
                                index,
                                depth: 0,
                                node: None,
                            });
                        }
                    }
                    PreludeImport::ModuleAsIs {
                        module: _,
                        bind,
                        res,
                        insert,
                    } => {
                        let id = self.symtab.id(&self.bintab.id_str(bind));
                        if let Ok(existing) = scope.resolve(id, self.epoch)
                            && existing.depth == 0
                        {
                            *insert = true;
                            *res = Some(existing);
                        } else {
                            let origin = Origin::PreludeModule;
                            let index = scope.insert(id, origin, self.epoch, false);
                            *res = Some(Res {
                                index,
                                depth: 0,
                                node: None,
                            });
                        }
                    }
                    PreludeImport::ModuleRenamed { bind, res, .. } => {
                        let id = self.symtab.id(&self.bintab.id_str(bind));
                        let origin = Origin::PreludeModule;
                        let index = scope.insert(id, origin, self.epoch, false);
                        *res = Some(Res {
                            index,
                            depth: 0,
                            node: None,
                        });
                    }
                }
            }
        }

        self.visit_block_inner(&mut scope, &mut node.body)?;

        if let Some(prelude) = &mut prelude {
            // Mark prelude items that were never read (by clearing resolution)
            for import in prelude.iter_mut() {
                match import {
                    PreludeImport::Items { items, .. } => {
                        for item in items.iter_mut() {
                            let res = item.res.as_ref().unwrap();
                            if !scope.is_read(res.index, res.depth) {
                                item.res = None
                            }
                        }
                    }
                    PreludeImport::ModuleAsIs { res, .. }
                    | PreludeImport::ModuleRenamed { res, .. } => {
                        let r = res.as_ref().unwrap();
                        if !scope.is_read(r.index, r.depth) {
                            *res = None
                        }
                    }
                }
            }
        }

        if prelude.is_some() && matches!(self.mode, Mode::Repl) {
            // Insert a binding for REPL variable (`_`)
            let id = self.symtab.id(&self.bintab.id_str("_"));
            let origin = Origin::Repl;
            let index = scope.insert(id, origin, self.epoch, false);
            node.body.repl = Some(Res {
                index,
                depth: 0,
                node: None,
            });
        }

        scope.finish(self, &mut node.body.vars);
        Ok(())
    }

    /// Elaborate a lambda body.
    fn visit_lambda(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut Function,
        badnl: Option<Span>,
    ) -> Result<()> {
        let mut scope = scope.lambda(badnl);
        for param in node.params.iter_mut() {
            self.visit_param_non_const_default(&mut scope, param)?;
            let ident = match param {
                Param::Pos { ident, .. }
                | Param::Key { ident, .. }
                | Param::ConstKey { ident, .. } => Some(ident),
                Param::Rest { ident, .. } => ident.as_mut(),
            };
            let Some(ident) = ident else {
                continue;
            };
            let sym = self
                .symtab
                .id(&self.bintab.id_str(self.file.str(ident.span)));
            let index = scope.insert(sym, Origin::Source(ident.span), self.epoch, false);
            ident.res = Some(Res {
                index,
                depth: 0,
                node: None,
            });
        }
        self.visit_block_inner(&mut scope, &mut node.body)?;
        scope.finish(self, &mut node.body.vars);
        Ok(())
    }

    pub(crate) fn new(
        mode: Mode<'a>,
        file: &'a File<'a>,
        bintab: &'a mut BinTable,
        symtab: &'a mut sym::Table,
        diags: &'a Diags,
    ) -> Self {
        Elaborater {
            mode,
            file,
            bintab,
            symtab,
            diags,
            fail: false,
            epoch: 0,
        }
    }

    /// Elaborate the AST in place.
    ///
    /// Errors are recorded rather than returned; consult [`Elaborater::failed`].
    pub(crate) fn elaborate(&mut self, root: &mut Root, prelude: &mut [PreludeImport]) {
        if self
            .visit_function(&mut Scope::new(), &mut root.0, Some(prelude))
            .is_err()
        {
            self.fail = true;
        }
        if matches!(self.mode, Mode::Module { .. } | Mode::Repl) {
            // Mark all exports as captured if not already
            for var in root.0.body.vars.iter_mut() {
                // In REPL mode, export *all* top-level bindings that aren't prelude imports
                if self.mode == Mode::Repl && !var.is_prelude() && !var.is_synthetic() {
                    var.exported = true;
                }

                if var.exported {
                    var.captured = true;
                }
            }
        }
    }

    /// Whether any error was recorded during elaboration
    pub(crate) fn failed(&self) -> bool {
        self.fail
    }
}
