use std::{
    cell::Cell,
    collections::HashMap,
    fmt::{self, Write},
    result,
};

use dolang_util::{alias, arena::ArenaVec, intern::BinTable};

use crate::{
    Compiler, Mode, PreludeImport,
    ast::{
        self, Arg, ArrayElem, Assign, Bind, Block, Class, Def, DictElem, Expand, Expr, ExprBody,
        For, Function, GetVariant, Ident, If, Import, ImportElement, ImportItem, Key, LValue, Let,
        Method, NlGuard, NlInfo, Pair, Param, Pattern, PatternBind, PrimStmt, Res, Return, Root,
        SideEffect, Single, Stmt, Try, Var, While, visit::Node,
    },
    diag::{AnnotationKind, Severity},
    doc::{self, Kind},
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
    doctab: &'a mut doc::Table,
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
        /// Document node this frame introduces, if it corresponds to a
        /// construct.  Frames without one are transparent to parentage.
        doc: Option<doc::Id>,
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
        doc: doc::Id,
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
        let span = resolver.doctab[var.node].kind.name()?;
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

    /// The document node enclosing anything declared in this scope.
    ///
    /// Frames that correspond to no construct — plain blocks, the base frame —
    /// are transparent, so a declaration inside one parents to whatever
    /// construct actually encloses it.
    fn doc_parent(&self) -> Option<doc::Id> {
        let mut scope = self;
        loop {
            match scope {
                Scope::Base => return None,
                Scope::Class { doc, .. } => return Some(*doc),
                Scope::Nested { doc, parent, .. } => match doc {
                    Some(doc) => return Some(*doc),
                    None => scope = parent,
                },
            }
        }
    }

    /// The document node of the nearest enclosing loop, for `break`/`continue`.
    fn doc_loop(&self) -> Option<doc::Id> {
        let mut scope = self;
        loop {
            match scope {
                Scope::Base => return None,
                Scope::Class { parent, .. } => scope = parent,
                Scope::Nested {
                    kind, doc, parent, ..
                } => match kind {
                    ScopeKind::Loop => return *doc,
                    // A jump cannot escape the function it is written in
                    ScopeKind::Function => return None,
                    _ => scope = parent,
                },
            }
        }
    }

    /// The document node of the nearest enclosing function, for `return`.
    ///
    /// A `return` inside a lambda returns from the enclosing *function*, so
    /// lambda frames are traversed rather than treated as the target.
    fn doc_function(&self) -> Option<doc::Id> {
        let mut scope = self;
        loop {
            match scope {
                Scope::Base => return None,
                Scope::Class { parent, .. } => scope = parent,
                Scope::Nested {
                    kind, doc, parent, ..
                } => match kind {
                    ScopeKind::Function => return *doc,
                    _ => scope = parent,
                },
            }
        }
    }

    fn nested(&'s self, doc: Option<doc::Id>) -> Self {
        Self::Nested {
            kind: ScopeKind::Normal,
            doc,
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

    fn nested_loop(&'s self, doc: Option<doc::Id>) -> Self {
        Self::Nested {
            kind: ScopeKind::Loop,
            doc,
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

    fn function(&'s self, doc: Option<doc::Id>, can_return: bool) -> Self {
        Self::Nested {
            kind: ScopeKind::Function,
            doc,
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

    fn class(&'s self, doc: doc::Id) -> Self {
        Self::Class {
            doc,
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

    fn mark_nl_return(&self, mut depth: usize, node: doc::Id, epoch: Epoch) -> usize {
        let mut scope = self;
        loop {
            scope = match scope {
                Scope::Base => unreachable!(),
                Scope::Class { parent, .. } => {
                    if depth == 0 {
                        return parent.insert_synthetic(node, epoch);
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
                            let index = scope.insert_synthetic(node, epoch);
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

    fn lambda(&'s self, doc: Option<doc::Id>, bad_nl: Option<Span>) -> Self {
        Self::Nested {
            kind: ScopeKind::Lambda,
            doc,
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

    fn insert(&mut self, sym: sym::Id, node: doc::Id, epoch: Epoch, exported: bool) -> usize {
        self.insert_with_lookup(sym, sym, node, epoch, exported)
    }

    fn insert_with_lookup(
        &mut self,
        lookup_sym: sym::Id,
        sym: sym::Id,
        node: doc::Id,
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
                        node,
                    },
                    epoch,
                )));
                index.insert(lookup_sym, i);
                i
            }
        }
    }

    fn insert_synthetic(&self, node: doc::Id, epoch: Epoch) -> usize {
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
                        node,
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

    fn resolve_inner(
        &self,
        id: sym::Id,
        capture: bool,
        promote: Option<doc::Id>,
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
                        if let Some(node) = promote {
                            var.node = node;
                        }
                        (var, epoch)
                    });
                    let (Var { node, .. }, _) = vars[index].get();
                    return Ok(Res {
                        index,
                        depth: 0,
                        node,
                    });
                }
                let Res { index, depth, node } = parent.resolve_inner(
                    id,
                    capture || matches!(kind, ScopeKind::Function | ScopeKind::Lambda),
                    promote,
                    epoch,
                )?;
                Ok(Res {
                    index,
                    depth: depth + 1,
                    node,
                })
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
        node: doc::Id,
        epoch: Epoch,
    ) -> result::Result<Res, ResolveError> {
        self.resolve_inner(id, false, Some(node), epoch)
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
    /// Record a document node parented to whatever construct `scope` is in.
    ///
    /// This is always called *before* pushing the scope the construct itself
    /// introduces, so a construct never parents to itself.
    fn doc(&mut self, scope: &Scope<'_>, kind: Kind, span: Span) -> doc::Id {
        let parent = scope.doc_parent();
        self.doctab.push(doc::Node::new(parent, kind, span))
    }

    /// Record decorator nodes parented to the declaration they decorate.
    ///
    /// A decorator is an ordinary node like any other.  When its expression is
    /// simply an identifier it also names the node that identifier resolves to,
    /// which is how a consumer recognizes a particular decorator — `#[getter]`,
    /// say — without matching source text.  Decorators must already have been
    /// resolved.
    fn doc_decorators(&mut self, parent: doc::Id, decorators: &[ast::Decorator]) {
        for decorator in decorators {
            let target = match &decorator.expr {
                Expr::Ident(ident) => ident.res.as_ref().map(|res| res.node),
                _ => None,
            };
            let span = decorator.open_span | decorator.close_span;
            self.doctab.push(doc::Node::new(
                Some(parent),
                Kind::Decorator { target },
                span,
            ));
        }
    }

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
                for_span,
                ..
            }) => {
                if let Some(expr) = expr {
                    self.visit_expr(scope, expr, false)?;
                }
                // Generate synthetic, unnameable variable to hold iterator
                let node = doc::Table::SYNTHETIC;
                let index = scope.insert_synthetic(node, self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    node,
                });
                {
                    // A comprehension `for` binds, so it needs a node of its
                    // own: without one, two sibling comprehensions in the same
                    // function would both parent to it and bind the same name.
                    let loop_node = self.doc(scope, Kind::ForElem, *for_span);
                    let mut scope = scope.nested_loop(Some(loop_node));
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
                for_span,
                ..
            }) => {
                if let Some(expr) = expr {
                    self.visit_expr(scope, expr, false)?;
                }
                // Generate synthetic, unnameable variable to hold iterator
                let node = doc::Table::SYNTHETIC;
                let index = scope.insert_synthetic(node, self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    node,
                });
                {
                    // A comprehension `for` binds, so it needs a node of its
                    // own: without one, two sibling comprehensions in the same
                    // function would both parent to it and bind the same name.
                    let loop_node = self.doc(scope, Kind::ForElem, *for_span);
                    let mut scope = scope.nested_loop(Some(loop_node));
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
                self.visit_lambda(
                    scope,
                    func,
                    Kind::Lambda,
                    span,
                    if is_arg { None } else { Some(span) },
                )
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
                            && matches!(self.doctab[res.node].kind, Kind::SelfParam { .. })
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
                    && matches!(self.doctab[res.node].kind, Kind::SelfParam { .. })
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
                for_span,
                ..
            }) => {
                if let Some(expr) = expr {
                    self.visit_expr(scope, expr, false)?;
                }
                // Generate synthetic, unnameable variable to hold iterator
                let node = doc::Table::SYNTHETIC;
                let index = scope.insert_synthetic(node, self.epoch);
                *iter = Some(Res {
                    index,
                    depth: 0,
                    node,
                });
                {
                    // A comprehension `for` binds, so it needs a node of its
                    // own: without one, two sibling comprehensions in the same
                    // function would both parent to it and bind the same name.
                    let loop_node = self.doc(scope, Kind::ForElem, *for_span);
                    let mut scope = scope.nested_loop(Some(loop_node));
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
        let node = self.doc(
            scope,
            Kind::Bind {
                name: ident.span,
                is_pub: export,
            },
            ident.span,
        );
        let index = scope.insert(id, node, self.epoch, export);
        ident.res = Some(Res {
            index,
            depth: 0,
            node,
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
            let doc_node = self.doc(
                scope,
                Kind::Bind {
                    name: ident.span,
                    is_pub: node.pub_span.is_some(),
                },
                ident.span,
            );
            let lookup_sym = self.symtab.id(&self.bintab.id_str(name));
            let index = scope.insert_with_lookup(lookup_sym, sym, doc_node, self.epoch, true);
            ident.res = Some(Res {
                index,
                depth: 0,
                node: doc_node,
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
        kind: Kind,
        span: Span,
        is_loop: bool,
    ) -> Result<()> {
        let branch_node = self.doc(scope, kind, span);
        let mut inner = if is_loop {
            scope.nested_loop(Some(branch_node))
        } else {
            scope.nested(Some(branch_node))
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
        let branch_node = self.doc(scope, Kind::IfElem, node.tbranch.span);
        self.visit_elem_branch_body(
            scope,
            branch_node,
            node.tbranch.bind.as_mut(),
            &mut node.tbranch.body,
            is_arg,
            visit_elem,
        )?;

        for (elif_branch, _) in &mut node.elif_branches {
            self.visit_expr(scope, &mut elif_branch.expr, false)?;
            let branch_node = self.doc(scope, Kind::IfElem, elif_branch.span);
            self.visit_elem_branch_body(
                scope,
                branch_node,
                elif_branch.bind.as_mut(),
                &mut elif_branch.body,
                is_arg,
                visit_elem,
            )?;
        }

        if let Some((else_body, else_span)) = &mut node.else_branch {
            let else_span = *else_span;
            let branch_node = self.doc(scope, Kind::Else, else_span);
            self.visit_elem_branch_body(scope, branch_node, None, else_body, is_arg, visit_elem)?;
        }

        Ok(())
    }

    /// The vertical-element counterpart of [`Self::visit_branch_body`].
    fn visit_elem_branch_body<T>(
        &mut self,
        scope: &mut Scope<'_>,
        branch_node: doc::Id,
        bind: Option<&mut PatternBind>,
        body: &mut ExprBody<T>,
        is_arg: bool,
        visit_elem: fn(&mut Self, &mut Scope<'_>, &mut T, bool) -> Result<()>,
    ) -> Result<()> {
        let mut inner = scope.nested(Some(branch_node));
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
        let span = node.while_span;
        self.visit_branch_body(
            scope,
            node.bind.as_mut(),
            &mut node.body,
            Kind::While,
            span,
            true,
        )
    }

    fn visit_if(&mut self, scope: &mut Scope<'_>, node: &mut If<Block>) -> Result<()> {
        // Visit first if branch
        self.visit_expr(scope, &mut node.tbranch.expr, false)?;
        let span = node.tbranch.span;
        self.visit_branch_body(
            scope,
            node.tbranch.bind.as_mut(),
            &mut node.tbranch.body,
            Kind::If,
            span,
            false,
        )?;

        // Visit elif branches
        for (elif_branch, _) in &mut node.elif_branches {
            self.visit_expr(scope, &mut elif_branch.expr, false)?;
            let span = elif_branch.span;
            self.visit_branch_body(
                scope,
                elif_branch.bind.as_mut(),
                &mut elif_branch.body,
                Kind::If,
                span,
                false,
            )?;
        }

        // Visit final else branch if present
        if let Some((else_block, else_span)) = &mut node.else_branch {
            let else_span = *else_span;
            self.visit_block(scope, else_block, Kind::Else, else_span)?;
        }

        Ok(())
    }

    /// Elaborate `try`.
    ///
    /// The body and each handler are elaborated as 0-parameter closures, but
    /// they are not lambdas in the source: each is its own construct, and each
    /// binds, so each gets a node.  Without one, two sibling handlers binding
    /// the same name would be indistinguishable to anything keyed on the pair
    /// of enclosing node and name.
    fn visit_try(&mut self, scope: &mut Scope<'_>, node: &mut Try) -> Result<()> {
        // Visit body as a function scope (0-param closure)
        let try_span = node.try_span;
        self.visit_lambda(scope, &mut node.body, Kind::Try, try_span, None)?;

        // For each handler: visit class_expr in outer scope, then handler func as function scope
        for handler in &mut node.handlers {
            if let Some(class_expr) = &mut handler.class_expr {
                self.visit_expr(scope, class_expr, false)?;
            }
            let span = handler.catch_span;
            self.visit_lambda(scope, &mut handler.func, Kind::Catch, span, None)?;
        }

        // Visit finally as function scope if present
        if let Some((finally_func, finally_span)) = &mut node.finally {
            let finally_span = *finally_span;
            self.visit_lambda(scope, finally_func, Kind::Finally, finally_span, None)?;
        }

        Ok(())
    }

    fn visit_import_pre(&mut self, scope: &mut Scope<'_>, import: &mut Import) -> Result<()> {
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
                    let name = self.module_span_first(*module);
                    let node = self.doc(
                        scope,
                        Kind::ImportModule {
                            module: *module,
                            name,
                        },
                        *module,
                    );
                    if let Ok(res) = scope.promote(id, node, self.epoch)
                        && res.depth == 0
                    {
                        // The name was already bound, so this import re-labels
                        // it rather than binding afresh.  The node the name
                        // previously resolved to stays in the table: it is a
                        // real declaration, just no longer the one in effect.
                        *insert = true;
                        bind.res = Some(res);
                    } else {
                        let index = scope.insert(id, node, self.epoch, false);
                        bind.res = Some(Res {
                            index,
                            depth: 0,
                            node,
                        });
                    }
                }
                ImportElement::ModuleRenamed { module, bind, .. } => {
                    let id = self
                        .symtab
                        .id(&self.bintab.id_str(self.file.str(bind.span)));
                    let node = self.doc(
                        scope,
                        Kind::ImportModule {
                            module: *module,
                            name: bind.span,
                        },
                        *module,
                    );
                    let index = scope.insert(id, node, self.epoch, false);
                    bind.res = Some(Res {
                        index,
                        depth: 0,
                        node,
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
                        let node = self.doc(
                            scope,
                            Kind::ImportItem {
                                module: *module,
                                item: item_span.unwrap_or(bind.span),
                                name: bind.span,
                            },
                            item_span.unwrap_or(bind.span),
                        );
                        let index = scope.insert(id, node, self.epoch, false);
                        bind.res = Some(Res {
                            index,
                            depth: 0,
                            node,
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
        let iter_node = doc::Table::SYNTHETIC;
        let index = scope.insert_synthetic(iter_node, self.epoch);
        node.iter = Some(Res {
            index,
            depth: 0,
            node: iter_node,
        });
        {
            let loop_node = self.doc(scope, Kind::For, node.for_span);
            let mut scope = scope.nested_loop(Some(loop_node));
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
        // Record the jump unconditionally: `NlInfo` below is only populated when
        // the jump crosses a function boundary, but a jump is navigable whether
        // or not it is non-local.
        let target = scope.doc_loop();
        self.doc(scope, Kind::Break { target }, span);
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
        let target = scope.doc_loop();
        self.doc(scope, Kind::Continue { target }, span);
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
        let target = scope.doc_function();
        self.doc(scope, Kind::Return { target }, *span);
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
                    let node = doc::Table::SYNTHETIC;
                    let index = scope.mark_nl_return(depth, node, self.epoch);
                    *nl = Some(NlInfo {
                        scope_depth: depth,
                        indicator: 3,
                        ret_upvar: Some(Res { index, depth, node }),
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
        // The class this belongs to is the parent link, so it need not be
        // recorded alongside.
        let kind = match &node.special {
            Some(_) => Kind::SpecialMethod {
                name: node.name_span,
            },
            None => Kind::Method {
                name: node.name_span,
                is_pub: node.pub_span.is_some(),
            },
        };
        node.node = Some(self.doc(scope, kind, node.def_span | node.name_span));
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
        let doc_node = self.doc_class(scope, node);
        let index = scope.insert(sym, doc_node, self.epoch, true);
        node.ident.res = Some(Res {
            index,
            depth: 0,
            node: doc_node,
        });
    }

    /// Record the document node for a class declaration.
    ///
    /// This runs from a pre-pass, before the superclass references have been
    /// resolved, so `supers` starts empty and [`Self::doc_class_supers`] fills
    /// it in once they resolve.
    fn doc_class(&mut self, scope: &Scope<'_>, class: &Class) -> doc::Id {
        self.doc(
            scope,
            Kind::Class {
                name: class.ident.span,
                is_pub: class.pub_span.is_some(),
                supers: alias::Box::default(),
            },
            class.class_span | class.ident.span,
        )
    }

    /// Record the resolved superclass references on a class node.
    ///
    /// A superclass reference is a use site rather than a child, so it cannot be
    /// expressed by parentage; naming the node it resolves to is what gives a
    /// consumer the import provenance.
    fn doc_class_supers(&mut self, class: &Class) {
        let Some(res) = class.ident.res else {
            return;
        };
        let supers = class
            .super_refs
            .iter()
            .map(|super_ref| doc::Super {
                // The span covers the whole reference, dotted path and all, so
                // a consumer can render it without stitching pieces together.
                span: super_ref
                    .fields
                    .last()
                    .map_or(super_ref.ident.span, |field| super_ref.ident.span | *field),
                // Only a bare identifier names a declaration; a path is an
                // expression, and the span is all a consumer gets.
                target: super_ref
                    .fields
                    .is_empty()
                    .then(|| super_ref.ident.res.as_ref().map(|res| res.node))
                    .flatten(),
            })
            .collect();
        if let Kind::Class { supers: slot, .. } = &mut self.doctab[res.node].kind {
            *slot = supers;
        }
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
        let doc_node = def.ident.res.as_ref().map(|res| res.node);
        if let Some(doc_node) = doc_node {
            self.doc_decorators(doc_node, &def.decorators);
        }
        self.visit_function(scope, &mut def.func, doc_node, None)
    }

    fn visit_method(&mut self, scope: &mut Scope<'_>, def: &mut Method) -> Result<()> {
        for decorator in &mut def.decorators {
            self.visit_expr(scope, &mut decorator.expr, false)?;
        }
        if let Some(doc_node) = def.node {
            self.doc_decorators(doc_node, &def.decorators);
        }
        self.visit_function(scope, &mut def.func, def.node, None)
    }

    /// Resolve a field decorator to a member-scope annotation.
    ///
    /// Fields have no runtime decorator semantics, so `class` and `static` are
    /// consumed here instead. Requiring the canonical prelude binding — rather
    /// than any expression that happens to be spelled `class` — keeps the door
    /// open for giving field decorators real semantics later.
    fn field_member_scope(&mut self, decorator: &ast::Decorator) -> Option<ast::MemberScope> {
        let ast::Expr::Ident(ident) = &decorator.expr else {
            return None;
        };
        let res = ident.res?;
        let Kind::PreludeItem { module, item, .. } = &self.doctab[res.node].kind else {
            return None;
        };
        if &**module != "std" {
            return None;
        }
        match &**item {
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
            match self.field_member_scope(decorator) {
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
            ast::FieldInit::Thunk(func) => self.visit_function(scope, func, None, None)?,
        }
        assert!(scope.is_class(), "class field outside class scope");
        let mut field_nodes = Vec::new();
        for field in &mut node.fields {
            // The class this belongs to is the parent link.  A declaration may
            // name several fields, which share the declaration's visibility and
            // decorators but each get their own node.
            let field_node = self.doc(
                scope,
                Kind::Field {
                    name: field.ident.span,
                    is_pub: node.pub_span.is_some(),
                },
                field.ident.span,
            );
            field.node = Some(field_node);
            field_nodes.push(field_node);
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
        // A field decorator is consumed by the elaborator, but a consumer still
        // needs to see it — that is how field scope is recognized without
        // matching source text — so it is recorded like any other decorator.
        for field_node in field_nodes {
            self.doc_decorators(field_node, &node.decorators);
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
                    let doc_node = self.doc(
                        scope,
                        Kind::Function {
                            name: ident_span,
                            is_pub: exported,
                        },
                        node.def_span | ident_span,
                    );
                    let index = scope.insert(sym, doc_node, self.epoch, exported);
                    node.ident.res = Some(Res {
                        index,
                        depth: 0,
                        node: doc_node,
                    });
                }
                Stmt::Class(node) => {
                    let sym = self
                        .symtab
                        .id(&self.bintab.id_str(self.file.str(node.ident.span)));
                    let exported = node.pub_span.is_some();
                    let doc_node = self.doc_class(scope, node);
                    let index = scope.insert(sym, doc_node, self.epoch, exported);
                    node.ident.res = Some(Res {
                        index,
                        depth: 0,
                        node: doc_node,
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

        // The superclass references have resolved by now, so they can name the
        // nodes they refer to.
        self.doc_class_supers(class);
        let doc_node = class.ident.res.as_ref().unwrap().node;
        self.doc_decorators(doc_node, &class.decorators);

        // Visit the class body in a new class scope
        {
            let mut class_scope = scope.class(doc_node);

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
                let node = doc::Table::SYNTHETIC;
                *stmt = Stmt::NlGuard(NlGuard {
                    body: Box::new(inner),
                    span,
                    has_break,
                    has_continue,
                    has_return: has_return.map(|index| Res {
                        index,
                        depth: 0,
                        node,
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
    fn visit_block(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut Block,
        kind: Kind,
        span: Span,
    ) -> Result<()> {
        let block_node = self.doc(scope, kind, span);
        let mut scope = scope.nested(Some(block_node));
        self.visit_block_inner(&mut scope, node)?;
        scope.finish(self, &mut node.vars);
        Ok(())
    }

    /// Describe a parameter for its document node: form, default span, extent.
    ///
    /// The parser splits key parameters into `:foo` and `foo: local` forms.
    /// Both are a key parameter to anything reading a signature, so they
    /// share a form and the parser stays free to re-split them.
    fn doc_param(param: &Param) -> (Kind, Span) {
        let (kind, key_span, ident, default) = match param {
            Param::Pos { ident, default } => (
                Kind::PositionalParam {
                    name: ident.span,
                    default: default.as_ref().map(|default| default.expr.span()),
                },
                None,
                Some(ident.span),
                default,
            ),
            Param::Key {
                key_span,
                ident,
                default,
            } => (
                Kind::KeyParam {
                    key: *key_span,
                    name: ident.span,
                    default: default.as_ref().map(|default| default.expr.span()),
                },
                Some(*key_span),
                Some(ident.span),
                default,
            ),
            Param::ConstKey {
                key_expr,
                ident,
                default,
                ..
            } => {
                let key = key_expr.span();
                (
                    Kind::KeyParam {
                        key,
                        name: ident.span,
                        default: default.as_ref().map(|default| default.expr.span()),
                    },
                    Some(key),
                    Some(ident.span),
                    default,
                )
            }
            Param::Rest {
                ellipsis_span,
                ident,
            } => (
                Kind::RestParam {
                    name: ident.as_ref().map(|ident| ident.span),
                },
                Some(*ellipsis_span),
                ident.as_ref().map(|ident| ident.span),
                &None,
            ),
        };
        let default_span = default.as_ref().map(|default| default.expr.span());
        let span = [key_span, ident, default_span]
            .into_iter()
            .flatten()
            .reduce(|acc, span| acc | span)
            .unwrap_or(Span::INVALID);
        (kind, span)
    }

    fn visit_function(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut Function,
        doc_node: Option<doc::Id>,
        mut prelude: Option<&mut [PreludeImport]>,
    ) -> Result<()> {
        let is_class_method = scope.is_class();
        let mut scope = scope.function(doc_node, self.mode != Mode::Repl || prelude.is_none());
        // Register all parameters as variables in this scope.
        // Visit non-constant default expressions before inserting each param,
        // so defaults can reference prior params but not the current or later ones.
        for (param_idx, param) in node.params.iter_mut().enumerate() {
            self.visit_param_non_const_default(&mut scope, param)?;
            let (param_kind, param_span) = Self::doc_param(param);
            // An anonymous `...` binds nothing, but it is still part of the
            // signature, so it gets a node with no name.
            let ident = match param {
                Param::Pos { ident, .. }
                | Param::Key { ident, .. }
                | Param::ConstKey { ident, .. } => Some(ident),
                Param::Rest { ident, .. } => ident.as_mut(),
            };
            let kind = if is_class_method && param_idx == 0 {
                Kind::SelfParam {
                    name: ident.as_ref().expect("`self` is always named").span,
                }
            } else {
                param_kind
            };
            let param_node = self.doc(&scope, kind, param_span);
            let Some(ident) = ident else {
                continue;
            };
            let sym = self
                .symtab
                .id(&self.bintab.id_str(self.file.str(ident.span)));
            let index = scope.insert(sym, param_node, self.epoch, false);
            ident.res = Some(Res {
                index,
                depth: 0,
                node: param_node,
            });
        }

        if let Some(prelude) = &mut prelude {
            for import in prelude.iter_mut() {
                match import {
                    PreludeImport::Items { module, items, .. } => {
                        for field in items.iter_mut() {
                            let id = self.symtab.id(&self.bintab.id_str(&field.bind));
                            let prelude_node = self.doc(
                                &scope,
                                Kind::PreludeItem {
                                    module: module.as_str().into(),
                                    item: field.item.as_str().into(),
                                    name: field.bind.as_str().into(),
                                },
                                Span::INVALID,
                            );
                            let index = scope.insert(id, prelude_node, self.epoch, false);
                            field.res = Some(Res {
                                index,
                                depth: 0,
                                node: prelude_node,
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
                            let prelude_node = self.doc(
                                &scope,
                                Kind::PreludeModule {
                                    module: module.as_str().into(),
                                    name: Self::module_name_first(module).into(),
                                },
                                Span::INVALID,
                            );
                            let index = scope.insert(id, prelude_node, self.epoch, false);
                            *res = Some(Res {
                                index,
                                depth: 0,
                                node: prelude_node,
                            });
                        }
                    }
                    PreludeImport::ModuleRenamed {
                        module, bind, res, ..
                    } => {
                        let id = self.symtab.id(&self.bintab.id_str(bind));
                        let prelude_node = self.doc(
                            &scope,
                            Kind::PreludeModule {
                                module: module.as_str().into(),
                                name: bind.as_str().into(),
                            },
                            Span::INVALID,
                        );
                        let index = scope.insert(id, prelude_node, self.epoch, false);
                        *res = Some(Res {
                            index,
                            depth: 0,
                            node: prelude_node,
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
                                // The import contributes nothing to this unit,
                                // so its node should not be surfaced either.
                                self.doctab[res.node].dead = true;
                                item.res = None
                            }
                        }
                    }
                    PreludeImport::ModuleAsIs { res, .. }
                    | PreludeImport::ModuleRenamed { res, .. } => {
                        let r = res.as_ref().unwrap();
                        if !scope.is_read(r.index, r.depth) {
                            self.doctab[r.node].dead = true;
                            *res = None
                        }
                    }
                }
            }
        }

        if prelude.is_some() && matches!(self.mode, Mode::Repl) {
            // Insert a binding for REPL variable (`_`)
            let id = self.symtab.id(&self.bintab.id_str("_"));
            let repl_node = doc::Table::REPL;
            let index = scope.insert(id, repl_node, self.epoch, false);
            node.body.repl = Some(Res {
                index,
                depth: 0,
                node: repl_node,
            });
        }

        scope.finish(self, &mut node.body.vars);
        Ok(())
    }

    /// Elaborate a lambda body.
    ///
    /// `kind` is what the construct is in the source: a `do` block is a
    /// [`Kind::Lambda`], but `try`/`catch`/`finally` bodies are elaborated as
    /// closures too and are their own constructs.  Either way the frame carries
    /// a node, so two sibling bodies that bind the same name stay distinct.
    fn visit_lambda(
        &mut self,
        scope: &mut Scope<'_>,
        node: &mut Function,
        kind: Kind,
        span: Span,
        badnl: Option<Span>,
    ) -> Result<()> {
        let doc_node = self.doc(scope, kind, span);
        let mut scope = scope.lambda(Some(doc_node), badnl);
        for param in node.params.iter_mut() {
            self.visit_param_non_const_default(&mut scope, param)?;
            let (param_kind, param_span) = Self::doc_param(param);
            // An anonymous `...` binds nothing, but it is still part of the
            // signature, so it gets a node with no name.
            let ident = match param {
                Param::Pos { ident, .. }
                | Param::Key { ident, .. }
                | Param::ConstKey { ident, .. } => Some(ident),
                Param::Rest { ident, .. } => ident.as_mut(),
            };
            let param_node = self.doc(&scope, param_kind, param_span);
            let Some(ident) = ident else {
                continue;
            };
            let sym = self
                .symtab
                .id(&self.bintab.id_str(self.file.str(ident.span)));
            let index = scope.insert(sym, param_node, self.epoch, false);
            ident.res = Some(Res {
                index,
                depth: 0,
                node: param_node,
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
        doctab: &'a mut doc::Table,
        diags: &'a Diags,
    ) -> Self {
        Elaborater {
            mode,
            file,
            bintab,
            symtab,
            doctab,
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
            .visit_function(&mut Scope::new(), &mut root.0, None, Some(prelude))
            .is_err()
        {
            self.fail = true;
        }
        if matches!(self.mode, Mode::Module { .. } | Mode::Repl) {
            // Mark all exports as captured if not already
            for var in root.0.body.vars.iter_mut() {
                // In REPL mode, export *all* top-level bindings that aren't prelude imports
                if self.mode == Mode::Repl && !var.is_prelude(self.doctab) && !var.is_synthetic() {
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
