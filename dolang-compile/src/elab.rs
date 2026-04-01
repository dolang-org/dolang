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
        self, Arg, ArrayElem, Assign, Bind, Block, Class, Def, DefVariant, DictElem, Expand, Expr,
        For, Function, GetVariant, Ident, If, Import, ImportElement, ImportItem, Key, LValue, Let,
        NlGuard, NlInfo, Pair, Param, Pattern, PrimStmt, Res, Return, SideEffect, Single, Stmt,
        Try, Unit, Var, While, visit::Node,
    },
    diag::{AnnotationKind, Severity},
    origin::{self, Origin},
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

#[derive(Clone)]
struct ImplicitUpvarReturn {
    span: Span,
    expr_span: Span,
}

impl Diagnose for ImplicitUpvarReturn {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "variable in implicit return position may be intended as a call"
        )
    }

    fn span(&self) -> Span {
        self.span
    }

    fn patches(&self) -> Box<dyn Iterator<Item = Box<dyn Patch>>> {
        Box::new(
            [
                Box::new(ImplicitUpvarReturnCallPatch {
                    span: self.expr_span,
                }) as Box<dyn Patch>,
                Box::new(ImplicitUpvarReturnExplicitPatch {
                    span: self.expr_span,
                }) as Box<dyn Patch>,
            ]
            .into_iter(),
        )
    }
}

struct ImplicitUpvarReturnCallPatch {
    span: Span,
}

impl Patch for ImplicitUpvarReturnCallPatch {
    fn span(&self) -> Span {
        self.span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "add `()` to make this a call")
    }

    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let original = compiler.file.str(self.span);
        write!(w, "{}()", original)
    }
}

struct ImplicitUpvarReturnExplicitPatch {
    span: Span,
}

impl Patch for ImplicitUpvarReturnExplicitPatch {
    fn span(&self) -> Span {
        self.span
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "add explicit `return`")
    }

    fn sub(&self, compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        let original = compiler.file.str(self.span);
        write!(w, "return {}", original)
    }
}

struct SpecialMethodOutsideClass(Span);

impl Diagnose for SpecialMethodOutsideClass {
    fn span(&self) -> Span {
        self.0
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "special methods are only valid in a class body")
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
    origintab: &'a mut origin::Table,
    fail: bool,
    epoch: Epoch,
}

enum ScopeKind {
    Normal,
    Lambda,
    Function,
    Class { class_span: Span },
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
        /// For class scopes: maps plain field name → unique private sym::Id
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
        let span = match resolver.origintab[var.origin] {
            // These origins can never result in a warning
            Origin::PreludeModule { .. }
            | Origin::Synthetic
            | Origin::Repl
            | Origin::PreludeItem { .. } => return None,
            // Extract span for others
            Origin::ImportItem { name, .. } => name,
            Origin::ImportModule { name, .. } => name,
            Origin::Class { span } | Origin::Def { span, .. } | Origin::Bind { span, .. } => span,
            Origin::Param { span } | Origin::SelfParam { span } => span,
        };
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
            Scope::Nested { can_break, .. } => *can_break,
        }
    }

    fn can_continue(&'s self) -> CanBranch {
        match self {
            Scope::Base => CanBranch::No,
            Scope::Nested { can_continue, .. } => *can_continue,
        }
    }

    fn can_return(&'s self) -> CanBranch {
        match self {
            Scope::Base => CanBranch::No,
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

    fn class_span(&self) -> Option<Span> {
        let mut scope = self;
        loop {
            match scope {
                Scope::Base => return None,
                Scope::Nested {
                    kind: ScopeKind::Class { class_span },
                    ..
                } => return Some(*class_span),
                Scope::Nested {
                    kind: ScopeKind::Function | ScopeKind::Lambda,
                    ..
                } => return None,
                Scope::Nested { parent, .. } => scope = parent,
            }
        }
    }

    fn is_class(&self) -> bool {
        matches!(
            self,
            Scope::Nested {
                kind: ScopeKind::Class { .. },
                ..
            }
        )
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
            class_private: HashMap::new(),
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
            class_private: HashMap::new(),
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
            class_private: HashMap::new(),
        }
    }

    fn class(&'s self, class_span: Span) -> Self {
        Self::Nested {
            kind: ScopeKind::Class { class_span },
            can_break: CanBranch::No,
            can_continue: CanBranch::No,
            can_return: CanBranch::No,
            nl_break: Cell::new(false),
            nl_continue: Cell::new(false),
            nl_return: Cell::new(None),
            vars: ArenaVec::new(),
            parent: self,
            index: HashMap::new(),
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
                        ScopeKind::Normal | ScopeKind::Class { .. } => (),
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
                        ScopeKind::Loop | ScopeKind::Normal | ScopeKind::Class { .. } => (),
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

    fn mark_nl_return(&self, mut depth: usize, origin: origin::Id, epoch: Epoch) -> usize {
        let mut scope = self;
        loop {
            scope = match scope {
                Scope::Base => unreachable!(),
                Scope::Nested {
                    nl_return, parent, ..
                } => {
                    if depth == 0 {
                        if let Some(index) = nl_return.get() {
                            return index;
                        } else {
                            let index = scope.insert_synthetic(origin, epoch);
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
            class_private: HashMap::new(),
        }
    }

    fn class_private_mut(&mut self) -> Option<&mut HashMap<String, sym::Id>> {
        match self {
            Scope::Nested {
                kind: ScopeKind::Class { .. },
                class_private,
                ..
            } => Some(class_private),
            _ => None,
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
                Scope::Nested {
                    kind,
                    class_private,
                    parent,
                    ..
                } => {
                    if matches!(kind, ScopeKind::Class { .. }) {
                        // Stop at the nearest enclosing class scope
                        return class_private.get(name).copied();
                    }
                    // Cross function/lambda/normal boundaries to reach the class scope
                    scope = parent;
                }
            }
        }
    }

    /// Returns true if `name` is registered as a private field in the nearest class scope.
    fn is_private_field(&self, name: &str) -> bool {
        self.lookup_private_field(name).is_some()
    }

    fn insert(&mut self, sym: sym::Id, origin: origin::Id, epoch: Epoch, exported: bool) -> usize {
        match self {
            Self::Base => panic!("Can't insert into base scope"),
            Self::Nested {
                kind, vars, index, ..
            } => {
                let i = vars.len();
                vars.push(Cell::new((
                    Var {
                        sym,
                        captured: false,
                        exported,
                        used: false,
                        origin,
                    },
                    epoch,
                )));
                // Class-scope vars are not lexically accessible — only via self.field
                if !matches!(kind, ScopeKind::Class { .. }) {
                    index.insert(sym, i);
                }
                i
            }
        }
    }

    fn insert_synthetic(&self, origin: origin::Id, epoch: Epoch) -> usize {
        match self {
            Self::Base => panic!("Can't insert into base scope"),
            Self::Nested { vars, .. } => {
                let i = vars.len();
                vars.push(Cell::new((
                    Var {
                        sym: sym::Id::new(usize::MAX),
                        captured: false,
                        exported: false,
                        used: true,
                        origin,
                    },
                    epoch,
                )));
                i
            }
        }
    }

    fn resolve_inner(
        &self,
        id: sym::Id,
        capture: bool,
        promote: Option<origin::Id>,
        epoch: Epoch,
    ) -> Result<Res> {
        match self {
            Scope::Base => Err(Error),
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
                    let (Var { origin, .. }, _) = vars[index].get();
                    return Ok(Res {
                        index,
                        depth: 0,
                        origin,
                    });
                }
                let Res {
                    index,
                    depth,
                    origin,
                } = parent.resolve_inner(
                    id,
                    capture || matches!(kind, ScopeKind::Function | ScopeKind::Lambda),
                    promote,
                    epoch,
                )?;
                Ok(Res {
                    index,
                    depth: depth + 1,
                    origin,
                })
            }
        }
    }

    fn is_read(&self, index: usize, depth: usize) -> bool {
        match self {
            Scope::Base => panic!("is_read on base scope"),
            Scope::Nested { vars, parent, .. } => {
                if depth == 0 {
                    vars[index].get().0.used
                } else {
                    parent.is_read(index, depth - 1)
                }
            }
        }
    }

    fn resolve(&self, id: sym::Id, epoch: Epoch) -> Result<Res> {
        self.resolve_inner(id, false, None, epoch)
    }

    fn promote(&self, id: sym::Id, origin: origin::Id, epoch: Epoch) -> Result<Res> {
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
            Err(Error) => {
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
            ArrayElem::If(node) => {
                // Visit first if branch
                self.visit_expr(scope, &mut node.tbranch.cond, is_arg)?;
                for elem in node.tbranch.body.iter_mut() {
                    let mut scope = scope.nested();
                    self.visit_array_elem(&mut scope, elem, is_arg)?;
                }

                // Visit elif branches
                for (elif_branch, _) in &mut node.elif_branches {
                    self.visit_expr(scope, &mut elif_branch.cond, is_arg)?;
                    for elem in elif_branch.body.iter_mut() {
                        let mut scope = scope.nested();
                        self.visit_array_elem(&mut scope, elem, is_arg)?;
                    }
                }

                // Visit final else branch if present
                if let Some((else_elems, _)) = &mut node.else_branch {
                    for elem in else_elems.iter_mut() {
                        let mut scope = scope.nested();
                        self.visit_array_elem(&mut scope, elem, is_arg)?;
                    }
                }

                Ok(())
            }
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
                let origin = self.origintab.id(&Origin::Synthetic);
                let index = scope.insert_synthetic(origin, self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    origin,
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
            DictElem::If(node) => {
                // Visit first if branch
                self.visit_expr(scope, &mut node.tbranch.cond, false)?;
                {
                    let mut nested_scope = scope.nested();
                    for elem in node.tbranch.body.iter_mut() {
                        self.visit_dict_elem(&mut nested_scope, elem, is_arg)?;
                    }
                }

                // Visit elif branches
                for (elif_branch, _) in &mut node.elif_branches {
                    self.visit_expr(scope, &mut elif_branch.cond, false)?;
                    let mut nested_scope = scope.nested();
                    for elem in elif_branch.body.iter_mut() {
                        self.visit_dict_elem(&mut nested_scope, elem, is_arg)?;
                    }
                }

                // Visit final else branch if present
                if let Some((else_elems, _)) = &mut node.else_branch {
                    let mut nested_scope = scope.nested();
                    for elem in else_elems.iter_mut() {
                        self.visit_dict_elem(&mut nested_scope, elem, is_arg)?;
                    }
                }

                Ok(())
            }
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
                let origin = self.origintab.id(&Origin::Synthetic);
                let index = scope.insert_synthetic(origin, self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    origin,
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
            Expr::Lambda { func, do_span, .. } => self.visit_lambda(
                scope,
                func,
                if is_arg {
                    None
                } else {
                    Some(do_span.unwrap_or_else(|| func.span()))
                },
            ),
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
                | Expr::I64(_, _)
                | Expr::VerbatimI64(_, _)
                | Expr::F64(_, _)
                | Expr::VerbatimF64(_, _)
                | Expr::Bool(_, _)
                | Expr::Nil(_)
                | Expr::Sym(_)
                | Expr::Array { .. }
                | Expr::Dict { .. }
                | Expr::Concat { .. }
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
                            && matches!(self.origintab[res.origin], Origin::SelfParam { .. })
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
            Expr::BinConcat { exprs, .. } => {
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
                    && matches!(self.origintab[res.origin], Origin::SelfParam { .. })
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
            Arg::If(node) => {
                // Visit first if branch
                self.visit_expr(scope, &mut node.tbranch.cond, false)?;
                {
                    let mut nested_scope = scope.nested();
                    for elem in node.tbranch.body.iter_mut() {
                        self.visit_cmd_arg(&mut nested_scope, elem)?;
                    }
                }

                // Visit elif branches
                for (elif_branch, _) in &mut node.elif_branches {
                    self.visit_expr(scope, &mut elif_branch.cond, false)?;
                    let mut nested_scope = scope.nested();
                    for elem in elif_branch.body.iter_mut() {
                        self.visit_cmd_arg(&mut nested_scope, elem)?;
                    }
                }

                // Visit final else branch if present
                if let Some((else_elems, _)) = &mut node.else_branch {
                    let mut nested_scope = scope.nested();
                    for elem in else_elems.iter_mut() {
                        self.visit_cmd_arg(&mut nested_scope, elem)?;
                    }
                }

                Ok(())
            }
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
                let origin = self.origintab.id(&Origin::Synthetic);
                let index = scope.insert_synthetic(origin, self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    origin,
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
        let origin = self.origintab.id(&Origin::Bind {
            span: ident.span,
            class: scope.class_span(),
        });
        let index = scope.insert(id, origin, self.epoch, export);
        ident.res = Some(Res {
            index,
            depth: 0,
            origin,
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
            let origin = self.origintab.id(&Origin::Bind {
                span: ident.span,
                class: scope.class_span(),
            });
            let index = scope.insert(sym, origin, self.epoch, true);
            ident.res = Some(Res {
                index,
                depth: 0,
                origin,
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

    fn visit_while(&mut self, scope: &mut Scope<'_>, node: &mut While) -> Result<()> {
        self.visit_expr(scope, &mut node.cond, false)?;
        let mut inner = scope.nested_loop();
        self.visit_block_inner(&mut inner, &mut node.body)?;
        inner.finish(self, &mut node.body.vars);
        Ok(())
    }

    fn visit_if(&mut self, scope: &mut Scope<'_>, node: &mut If<Block>) -> Result<()> {
        // Visit first if branch
        self.visit_expr(scope, &mut node.tbranch.cond, false)?;
        self.visit_block(scope, &mut node.tbranch.body)?;

        // Visit elif branches
        for (elif_branch, _) in &mut node.elif_branches {
            self.visit_expr(scope, &mut elif_branch.cond, false)?;
            self.visit_block(scope, &mut elif_branch.body)?;
        }

        // Visit final else branch if present
        if let Some((else_block, _)) = &mut node.else_branch {
            self.visit_block(scope, else_block)?;
        }

        Ok(())
    }

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

    fn visit_import(&mut self, scope: &mut Scope<'_>, import: &mut Import) -> Result<()> {
        for element in import.0.iter_mut() {
            match element {
                ImportElement::ModuleAsIs {
                    module,
                    bind,
                    insert,
                } => {
                    let id = self
                        .symtab
                        .id(&self.bintab.id_str(self.file.str(bind.span)));
                    let origin = self.origintab.id(&Origin::ImportModule {
                        module: *module,
                        name: self.module_span_first(*module),
                    });
                    if let Ok(res) = scope.promote(id, origin, self.epoch)
                        && res.depth == 0
                    {
                        *insert = true;
                        bind.res = Some(res);
                    } else {
                        let index = scope.insert(id, origin, self.epoch, false);
                        bind.res = Some(Res {
                            index,
                            depth: 0,
                            origin,
                        });
                    }
                }
                ImportElement::ModuleRenamed { module, bind, .. } => {
                    let id = self
                        .symtab
                        .id(&self.bintab.id_str(self.file.str(bind.span)));
                    let origin = self.origintab.id(&Origin::ImportModule {
                        module: *module,
                        name: bind.span,
                    });
                    let index = scope.insert(id, origin, self.epoch, false);
                    bind.res = Some(Res {
                        index,
                        depth: 0,
                        origin,
                    });
                }
                ImportElement::Items { module, items } => {
                    assert!(!items.is_empty());
                    for item in items.iter_mut() {
                        let (item_span, bind) = match item {
                            ImportItem::AsIs { bind, .. } => (None, bind),
                            ImportItem::Renamed { item, bind, .. } => (Some(*item), bind),
                        };
                        let id = self
                            .symtab
                            .id(&self.bintab.id_str(self.file.str(bind.span)));
                        let origin = self.origintab.id(&Origin::ImportItem {
                            module: *module,
                            item: item_span.unwrap_or(bind.span),
                            name: bind.span,
                        });
                        let index = scope.insert(id, origin, self.epoch, false);
                        bind.res = Some(Res {
                            index,
                            depth: 0,
                            origin,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn visit_for(&mut self, scope: &mut Scope<'_>, node: &mut For<Block>) -> Result<()> {
        if let Some(expr) = &mut node.expr {
            self.visit_expr(scope, expr, false)?;
        }
        // Generate synthetic, unnameable variable to hold iterator
        let origin = self.origintab.id(&Origin::Synthetic);
        let index = scope.insert_synthetic(origin, self.epoch);
        node.iter = Some(Res {
            index,
            depth: 0,
            origin,
        });
        {
            let mut scope = scope.nested_loop();
            // Inject loop binds into inner scope
            match &mut node.bind {
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
                self.visit_expr(scope, cmd, false)?;
                // Post-visit check: if this is the final statement and it's a non-local variable,
                // warn that it might be intended as a call
                if is_final
                    && let Expr::Ident(ident) = cmd
                    && let Some(res) = &ident.res
                    && res.depth > 0
                    && matches!(
                        self.origintab[res.origin],
                        Origin::Def { .. } | Origin::PreludeItem { .. }
                    )
                {
                    self.diags.push(ImplicitUpvarReturn {
                        span: cmd.span(),
                        expr_span: cmd.span(),
                    });
                }
                Ok(())
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
                    let origin = self.origintab.id(&Origin::Synthetic);
                    let index = scope.mark_nl_return(depth, origin, self.epoch);
                    *nl = Some(NlInfo {
                        scope_depth: depth,
                        indicator: 3,
                        ret_upvar: Some(Res {
                            index,
                            depth,
                            origin,
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

    fn visit_def(&mut self, scope: &mut Scope<'_>, def: &mut Def) -> Result<()> {
        // Check pub validity
        if let Some(span) = def.pub_span
            && !scope.is_top_level()
            && !scope.is_class()
        {
            self.diags.push(InappropriatePub(span));
            self.fail = true;
        }
        // All scope insertion is handled by visit_body_pre; just visit the body.
        self.visit_function(scope, &mut def.func, None)
    }

    fn visit_body_pre(&mut self, scope: &mut Scope<'_>, block: &mut Block) -> Result<()> {
        for stmt in block.stmts.iter_mut() {
            if scope.is_class() {
                match stmt {
                    Stmt::Let(node) if node.pub_span.is_none() => {
                        // Register private field symbol for non-pub let.
                        if let ast::Pattern::Ident(ident) = &node.bind {
                            let name = self.file.str(ident.span).to_owned();
                            let private_sym = self.symtab.fresh(self.bintab.id_str(&name));
                            scope.class_private_mut().unwrap().insert(name, private_sym);
                        }
                    }
                    Stmt::Def(node) => {
                        // Register private sym for non-pub Normal def before inserting,
                        // so lookup_private_field finds it when computing effective_sym.
                        if node.pub_span.is_none()
                            && let DefVariant::Normal(ident) = &node.variant
                        {
                            let name = self.file.str(ident.span).to_owned();
                            let private_sym = self.symtab.fresh(self.bintab.id_str(&name));
                            scope.class_private_mut().unwrap().insert(name, private_sym);
                        }
                        // Insert the def. All class members are exported; private Normal
                        // defs use their unique sym.
                        let (sym, origin) = match &node.variant {
                            DefVariant::Normal(ident) => (
                                self.symtab
                                    .id(&self.bintab.id_str(self.file.str(ident.span))),
                                self.origintab.id(&Origin::Def {
                                    span: ident.span,
                                    class: scope.class_span(),
                                }),
                            ),
                            DefVariant::Special(method, span, _) => (
                                self.symtab.id(&self.bintab.id_str(method.sym())),
                                self.origintab.id(&Origin::Def {
                                    span: *span,
                                    class: scope.class_span(),
                                }),
                            ),
                        };
                        let effective_sym = if node.pub_span.is_none() {
                            if let DefVariant::Normal(ident) = &node.variant {
                                let name = self.file.str(ident.span).to_owned();
                                scope.lookup_private_field(&name).unwrap_or(sym)
                            } else {
                                sym
                            }
                        } else {
                            sym
                        };
                        let index = scope.insert(effective_sym, origin, self.epoch, true);
                        let res = match &mut node.variant {
                            DefVariant::Normal(ident) => &mut ident.res,
                            DefVariant::Special(_, _, res) => res,
                        };
                        *res = Some(Res {
                            index,
                            depth: 0,
                            origin,
                        });
                    }
                    Stmt::Class(node) => {
                        let sym = self
                            .symtab
                            .id(&self.bintab.id_str(self.file.str(node.ident.span)));
                        let origin = self.origintab.id(&Origin::Class {
                            span: node.ident.span,
                        });
                        let index = scope.insert(sym, origin, self.epoch, true);
                        node.ident.res = Some(Res {
                            index,
                            depth: 0,
                            origin,
                        });
                    }
                    _ => {}
                }
            } else {
                match stmt {
                    Stmt::Def(node) => {
                        // Non-class scope: insert all defs for forward reference support
                        // (enables recursive and co-recursive function definitions).
                        match &mut node.variant {
                            DefVariant::Normal(ident) => {
                                let sym = self
                                    .symtab
                                    .id(&self.bintab.id_str(self.file.str(ident.span)));
                                let origin = self.origintab.id(&Origin::Def {
                                    span: ident.span,
                                    class: scope.class_span(),
                                });
                                let exported = node.pub_span.is_some();
                                let index = scope.insert(sym, origin, self.epoch, exported);
                                ident.res = Some(Res {
                                    index,
                                    depth: 0,
                                    origin,
                                });
                            }
                            DefVariant::Special(_, span, _) => {
                                self.fail = true;
                                self.diags.push(SpecialMethodOutsideClass(*span));
                            }
                        }
                    }
                    Stmt::Class(node) => {
                        let sym = self
                            .symtab
                            .id(&self.bintab.id_str(self.file.str(node.ident.span)));
                        let origin = self.origintab.id(&Origin::Class {
                            span: node.ident.span,
                        });
                        let exported = node.pub_span.is_some();
                        let index = scope.insert(sym, origin, self.epoch, exported);
                        node.ident.res = Some(Res {
                            index,
                            depth: 0,
                            origin,
                        });
                    }
                    _ => {}
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

        // Resolve superclass expressions BEFORE inserting the class name
        // (the class name should not be available in its own superclass expressions)
        for super_expr in &mut class.super_exprs {
            self.visit_expr(scope, super_expr, false)?;
        }

        assert!(
            class.ident.res.is_some(),
            "class should already be registered during block pre-pass"
        );

        // Visit the class body in a new class scope
        {
            let mut class_scope = scope.class(class.ident.span);

            self.visit_block_inner(&mut class_scope, &mut class.body)?;
            class_scope.finish(self, &mut class.body.vars);
        }

        // Mark ALL variables in the class body as captured and exported
        // (they will be packed into the reified scope).
        // Private fields are exported under their private sym;
        // public fields and special methods are exported under their plain sym.
        for var in class.body.vars.iter_mut() {
            var.captured = true;
            var.exported = true;
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
                let origin = self.origintab.id(&Origin::Synthetic);
                *stmt = Stmt::NlGuard(NlGuard {
                    body: Box::new(inner),
                    span,
                    has_break,
                    has_continue,
                    has_return: has_return.map(|index| Res {
                        index,
                        depth: 0,
                        origin,
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
                | Param::ConstKey { ident, .. } => ident,
                Param::Rest { ident, .. } => {
                    if let Some(ident) = ident {
                        ident
                    } else {
                        continue;
                    }
                }
            };
            let sym = self
                .symtab
                .id(&self.bintab.id_str(self.file.str(ident.span)));
            let origin = if is_class_method && param_idx == 0 {
                self.origintab.id(&Origin::SelfParam { span: ident.span })
            } else {
                self.origintab.id(&Origin::Param { span: ident.span })
            };
            let index = scope.insert(sym, origin, self.epoch, false);
            ident.res = Some(Res {
                index,
                depth: 0,
                origin,
            });
        }

        if let Some(prelude) = &mut prelude {
            for import in prelude.iter_mut() {
                match import {
                    PreludeImport::Items { module, items, .. } => {
                        for field in items.iter_mut() {
                            let id = self.symtab.id(&self.bintab.id_str(&field.bind));
                            let origin = self.origintab.id(&Origin::PreludeItem {
                                module: module.clone(),
                                item: field.item.clone(),
                                name: field.bind.clone(),
                            });
                            let index = scope.insert(id, origin, self.epoch, false);
                            field.res = Some(Res {
                                index,
                                depth: 0,
                                origin,
                            });
                        }
                    }
                    PreludeImport::ModuleAsIs {
                        module,
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
                            let origin = self.origintab.id(&Origin::PreludeModule {
                                module: module.clone(),
                                name: Self::module_name_first(module).to_string(),
                            });
                            let index = scope.insert(id, origin, self.epoch, false);
                            *res = Some(Res {
                                index,
                                depth: 0,
                                origin,
                            });
                        }
                    }
                    PreludeImport::ModuleRenamed {
                        module, bind, res, ..
                    } => {
                        let id = self.symtab.id(&self.bintab.id_str(bind));
                        let origin = self.origintab.id(&Origin::PreludeModule {
                            module: module.clone(),
                            name: bind.clone(),
                        });
                        let index = scope.insert(id, origin, self.epoch, false);
                        *res = Some(Res {
                            index,
                            depth: 0,
                            origin,
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
            let origin = self.origintab.id(&Origin::Repl);
            let index = scope.insert(id, origin, self.epoch, false);
            node.body.repl = Some(Res {
                index,
                depth: 0,
                origin,
            });
        }

        scope.finish(self, &mut node.body.vars);
        Ok(())
    }

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
                | Param::ConstKey { ident, .. } => ident,
                Param::Rest { ident, .. } => {
                    if let Some(ident) = ident {
                        ident
                    } else {
                        continue;
                    }
                }
            };
            let sym = self
                .symtab
                .id(&self.bintab.id_str(self.file.str(ident.span)));
            let origin = self.origintab.id(&Origin::Param { span: ident.span });
            let index = scope.insert(sym, origin, self.epoch, false);
            ident.res = Some(Res {
                index,
                depth: 0,
                origin,
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
        origintab: &'a mut origin::Table,
        diags: &'a Diags,
    ) -> Self {
        Elaborater {
            mode,
            file,
            bintab,
            symtab,
            origintab,
            diags,
            fail: false,
            epoch: 0,
        }
    }

    pub(crate) fn elaborate(
        &mut self,
        root: &mut Unit,
        prelude: &mut [PreludeImport],
        ignore_error: bool,
    ) -> Result<()> {
        let res = self
            .visit_function(&mut Scope::new(), &mut root.0, Some(prelude))
            .and(if self.fail { Err(Error) } else { Ok(()) });
        if matches!(self.mode, Mode::Module { .. } | Mode::Repl) {
            // Mark all exports as captured if not already
            for var in root.0.body.vars.iter_mut() {
                // In REPL mode, export *all* top-level bindings that aren't prelude imports
                if self.mode == Mode::Repl
                    && !var.is_prelude(self.origintab)
                    && !var.is_synthetic(self.origintab)
                {
                    var.exported = true;
                }

                if var.exported {
                    var.captured = true;
                }
            }
        }
        if !ignore_error {
            res?
        }
        Ok(())
    }
}
