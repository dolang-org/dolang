use super::{validate::Invalid, *};
use crate::typeck::r#type::Literal;

fn expr(kind: ExprKind) -> Expr {
    Expr {
        kind,
        span: Span::INVALID,
    }
}

fn module(graph: &Graph) -> FuncId {
    graph.alloc_func(FuncKind::Module(UnitId::from_index(0)), None)
}

fn closure(graph: &Graph, parent: FuncId) -> FuncId {
    graph.alloc_func(FuncKind::Decl(DeclId::from_index(0)), Some(parent))
}

fn terminate(graph: &Graph, block: BlockId, terminal: Terminal) {
    graph.block_mut(block).terminal = terminal;
}

fn push(graph: &Graph, block: BlockId, step: Step) {
    graph.block_mut(block).steps.push(step);
}

fn call(graph: &Graph, callee: Expr, args: Vec<Item>) -> Expr {
    expr(ExprKind::Call {
        callee: Box::new(callee),
        args,
        rule: graph.alloc_rule(),
    })
}

/// Assign a function's result, as a return does before continuing to its exit
fn result(graph: &Graph, func: FuncId, value: ExprKind) -> Step {
    Step::Assign {
        target: Target::Var(graph.func(func).result),
        value: expr(value),
    }
}

/// A module whose entry block returns nil
fn returning(graph: &Graph) -> FuncId {
    let func = module(graph);
    let (entry, exit) = {
        let func = graph.func(func);
        (func.entry, func.exit)
    };
    push(
        graph,
        entry,
        result(graph, func, ExprKind::Literal(Literal::Nil)),
    );
    terminate(graph, entry, Terminal::Branch(exit));
    func
}

#[test]
fn well_formed() {
    let graph = Graph::new();
    let top = module(&graph);
    let (entry, exit) = {
        let func = graph.func(top);
        (func.entry, func.exit)
    };
    let x = graph.alloc_var(top, Origin::Source(Span::INVALID), None);
    let f = graph.alloc_var(top, Origin::Source(Span::INVALID), None);

    // `f(x, x && g(x))`, with `x` captured by the closure `g`
    let lambda = closure(&graph, top);
    let (lambda_entry, lambda_exit) = {
        let mut func = graph.func_mut(lambda);
        func.captures.push(x);
        (func.entry, func.exit)
    };
    graph.var_mut(x).captured = true;
    push(
        &graph,
        lambda_entry,
        result(&graph, lambda, ExprKind::Var(x)),
    );
    terminate(&graph, lambda_entry, Terminal::Branch(lambda_exit));

    // Only the short circuit spills: `f` and the first `x` stay in the tree
    push(&graph, entry, Step::Push(expr(ExprKind::Var(x))));
    push(&graph, entry, Step::Dup);
    let rhs = graph.alloc_block(top, None, 0);
    let join = graph.alloc_block(top, None, 0);
    terminate(
        &graph,
        entry,
        Terminal::If {
            cond: expr(ExprKind::Operand),
            then: rhs,
            else_: join,
        },
    );
    push(&graph, rhs, Step::Pop);
    let g = call(&graph, expr(ExprKind::Lambda(lambda)), Vec::new());
    push(&graph, rhs, Step::Push(g));
    terminate(&graph, rhs, Terminal::Branch(join));

    // `try … finally`, whose body calls and falls through
    let value = call(
        &graph,
        expr(ExprKind::Var(f)),
        vec![
            Item::Pos(expr(ExprKind::Var(x))),
            Item::Pos(expr(ExprKind::Operand)),
        ],
    );
    push(&graph, join, Step::Eval(value));
    let after = graph.alloc_block(top, None, 0);
    let finally = graph.alloc_block(top, None, 1);
    terminate(
        &graph,
        join,
        Terminal::Leave {
            entry: finally,
            tag: Tag::Goto(after),
        },
    );
    terminate(&graph, finally, Terminal::EndFinally);
    push(&graph, after, result(&graph, top, ExprKind::Var(x)));
    terminate(&graph, after, Terminal::Branch(exit));

    let ir = graph.freeze();
    assert_eq!(ir.validate(), Ok(()));
    assert_eq!(ir.var(x).readers, [lambda]);
    assert!(ir.var(f).readers.is_empty());
}

#[test]
fn non_local() {
    let graph = Graph::new();
    let top = module(&graph);
    let (entry, exit) = {
        let func = graph.func(top);
        (func.entry, func.exit)
    };
    // `try each items do return 1 finally …`: the guard point's phantom edge to
    // the return leaves through the `finally` with a bottom result, joined at the
    // exit with the value that the closure returns straight to it
    let lambda = closure(&graph, top);
    let lambda_entry = graph.func(lambda).entry;
    terminate(
        &graph,
        lambda_entry,
        Terminal::ReturnFrom {
            func: top,
            value: expr(ExprKind::Literal(Literal::Int(1))),
        },
    );

    let finally = graph.alloc_block(top, None, 1);
    terminate(&graph, finally, Terminal::EndFinally);
    let call_block = graph.alloc_block(top, None, 0);
    let returned = graph.alloc_block(top, None, 0);
    terminate(
        &graph,
        entry,
        Terminal::Guard {
            next: call_block,
            targets: vec![returned],
        },
    );
    push(&graph, returned, result(&graph, top, ExprKind::Never));
    terminate(
        &graph,
        returned,
        Terminal::Leave {
            entry: finally,
            tag: Tag::Goto(exit),
        },
    );
    let each = call(&graph, expr(ExprKind::Lambda(lambda)), Vec::new());
    push(&graph, call_block, Step::Eval(each));
    push(
        &graph,
        call_block,
        result(&graph, top, ExprKind::Literal(Literal::Nil)),
    );
    terminate(
        &graph,
        call_block,
        Terminal::Leave {
            entry: finally,
            tag: Tag::Goto(exit),
        },
    );

    // `for x = xs` around `each items do break`
    let func = graph.alloc_func(FuncKind::Decl(DeclId::from_index(1)), Some(top));
    let (func_entry, func_exit) = {
        let func = graph.func(func);
        (func.entry, func.exit)
    };
    let iter = graph.alloc_var(func, Origin::Synthetic, None);
    let header = graph.alloc_block(func, None, 0);
    let body = graph.alloc_block(func, None, 0);
    let call_block = graph.alloc_block(func, None, 0);
    let loop_exit = graph.alloc_block(func, None, 0);
    push(
        &graph,
        func_entry,
        Step::Assign {
            target: Target::Var(iter),
            value: expr(ExprKind::Literal(Literal::Nil)),
        },
    );
    terminate(&graph, func_entry, Terminal::Branch(header));
    terminate(
        &graph,
        header,
        Terminal::Next {
            iter,
            pattern: Pattern::Unpack(Vec::new()),
            body,
            exit: loop_exit,
            span: Span::INVALID,
        },
    );
    terminate(
        &graph,
        body,
        Terminal::Guard {
            next: call_block,
            targets: vec![loop_exit],
        },
    );
    let breaking = closure(&graph, func);
    terminate(&graph, graph.func(breaking).entry, Terminal::Escape);
    let each = call(&graph, expr(ExprKind::Lambda(breaking)), Vec::new());
    push(&graph, call_block, Step::Eval(each));
    terminate(&graph, call_block, Terminal::Branch(header));
    push(
        &graph,
        loop_exit,
        result(&graph, func, ExprKind::Literal(Literal::Nil)),
    );
    terminate(&graph, loop_exit, Terminal::Branch(func_exit));
    assert_eq!(graph.freeze().validate(), Ok(()));
}

#[test]
fn non_local_outside_closure() {
    // Escaping from top-level code
    let graph = Graph::new();
    let top = returning(&graph);
    let block = graph.alloc_block(top, None, 0);
    terminate(&graph, block, Terminal::Escape);
    assert_eq!(graph.freeze().validate(), Err(Invalid::NonLocal(block)));

    // Returning from a function that doesn't enclose the closure
    let graph = Graph::new();
    let top = returning(&graph);
    let first = closure(&graph, top);
    let second = closure(&graph, top);
    let entry = graph.func(second).entry;
    terminate(
        &graph,
        entry,
        Terminal::ReturnFrom {
            func: first,
            value: expr(ExprKind::Literal(Literal::Nil)),
        },
    );
    assert_eq!(graph.freeze().validate(), Err(Invalid::NonLocal(entry)));
}

#[test]
fn foreign_edge() {
    let graph = Graph::new();
    let top = returning(&graph);
    let lambda = closure(&graph, top);
    let lambda_entry = graph.func(lambda).entry;
    // A function's block can't branch into a closure it encloses
    let entry = graph.func(top).entry;
    terminate(&graph, entry, Terminal::Branch(lambda_entry));
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::ForeignEdge {
            from: entry,
            to: lambda_entry
        })
    );

    // Nor can a closure branch to its enclosing function
    let graph = Graph::new();
    let top = returning(&graph);
    let lambda = closure(&graph, top);
    let lambda_entry = graph.func(lambda).entry;
    let exit = graph.func(top).exit;
    terminate(&graph, lambda_entry, Terminal::Branch(exit));
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::ForeignEdge {
            from: lambda_entry,
            to: exit
        })
    );
}

#[test]
fn finally_depths() {
    // Branching into a `finally` body
    let graph = Graph::new();
    let top = returning(&graph);
    let entry = graph.func(top).entry;
    let finally = graph.alloc_block(top, None, 1);
    terminate(&graph, finally, Terminal::EndFinally);
    terminate(&graph, entry, Terminal::Branch(finally));
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::Deeper {
            from: entry,
            to: finally
        })
    );

    // Leaving two levels at once
    let graph = Graph::new();
    let top = returning(&graph);
    let (entry, exit) = {
        let func = graph.func(top);
        (func.entry, func.exit)
    };
    let finally = graph.alloc_block(top, None, 2);
    terminate(&graph, finally, Terminal::EndFinally);
    terminate(
        &graph,
        entry,
        Terminal::Leave {
            entry: finally,
            tag: Tag::Goto(exit),
        },
    );
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::LeaveDepth {
            from: entry,
            to: finally
        })
    );

    // Continuing into a `finally` body the source isn't in
    let graph = Graph::new();
    let top = returning(&graph);
    let entry = graph.func(top).entry;
    let finally = graph.alloc_block(top, None, 1);
    terminate(&graph, finally, Terminal::EndFinally);
    terminate(
        &graph,
        entry,
        Terminal::Leave {
            entry: finally,
            tag: Tag::Goto(finally),
        },
    );
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::LeaveDepth {
            from: entry,
            to: finally
        })
    );

    // Ending a `finally` outside one
    let graph = Graph::new();
    let top = returning(&graph);
    let block = graph.alloc_block(top, None, 0);
    terminate(&graph, block, Terminal::EndFinally);
    assert_eq!(graph.freeze().validate(), Err(Invalid::EndOutside(block)));

    // A handler inside a `finally` body the block isn't in
    let graph = Graph::new();
    let top = returning(&graph);
    let handler = graph.alloc_block(top, None, 1);
    terminate(&graph, handler, Terminal::EndFinally);
    let block = graph.alloc_block(top, Some(handler), 0);
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::Deeper {
            from: block,
            to: handler
        })
    );
}

#[test]
fn returns() {
    // Returning other than from the exit block
    let graph = Graph::new();
    let top = returning(&graph);
    let block = graph.alloc_block(top, None, 0);
    terminate(&graph, block, Terminal::Return);
    assert_eq!(graph.freeze().validate(), Err(Invalid::Return(block)));

    // An exit block that doesn't return
    let graph = Graph::new();
    let top = returning(&graph);
    let exit = graph.func(top).exit;
    terminate(&graph, exit, Terminal::Unreachable);
    assert_eq!(graph.freeze().validate(), Err(Invalid::Return(exit)));
}

#[test]
fn variables() {
    // An enclosing function's variable that the closure doesn't list as a capture
    let graph = Graph::new();
    let top = returning(&graph);
    let x = graph.alloc_var(top, Origin::Source(Span::INVALID), None);
    let lambda = closure(&graph, top);
    let (lambda_entry, lambda_exit) = {
        let func = graph.func(lambda);
        (func.entry, func.exit)
    };
    push(&graph, lambda_entry, Step::Push(expr(ExprKind::Var(x))));
    terminate(&graph, lambda_entry, Terminal::Branch(lambda_exit));
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::Var {
            func: lambda,
            var: x
        })
    );

    // A closure's variable, used by its parent
    let graph = Graph::new();
    let top = returning(&graph);
    let lambda = closure(&graph, top);
    let y = graph.alloc_var(lambda, Origin::Synthetic, None);
    let entry = graph.func(top).entry;
    graph.block_mut(entry).steps.insert(
        0,
        Step::Assign {
            target: Target::Var(y),
            value: expr(ExprKind::Literal(Literal::Nil)),
        },
    );
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::Var { func: top, var: y })
    );

    // A parameter owned by another function
    let graph = Graph::new();
    let top = returning(&graph);
    let x = graph.alloc_var(top, Origin::Source(Span::INVALID), None);
    let lambda = closure(&graph, top);
    graph.func_mut(lambda).params = Pattern::Bind(x);
    let lambda_exit = graph.func(lambda).exit;
    let lambda_entry = graph.func(lambda).entry;
    terminate(&graph, lambda_entry, Terminal::Branch(lambda_exit));
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::Param {
            func: lambda,
            var: x
        })
    );
}

#[test]
fn lambdas_and_rules() {
    // A closure instantiated by its grandparent
    let graph = Graph::new();
    let top = returning(&graph);
    let middle = closure(&graph, top);
    let inner = closure(&graph, middle);
    for func in [middle, inner] {
        let (entry, exit) = {
            let func = graph.func(func);
            (func.entry, func.exit)
        };
        terminate(&graph, entry, Terminal::Branch(exit));
    }
    let entry = graph.func(top).entry;
    graph
        .block_mut(entry)
        .steps
        .insert(0, Step::Eval(expr(ExprKind::Lambda(inner))));
    assert_eq!(
        graph.freeze().validate(),
        Err(Invalid::Lambda {
            func: top,
            lambda: inner
        })
    );

    // A rule used twice
    let graph = Graph::new();
    let top = returning(&graph);
    let entry = graph.func(top).entry;
    let rule = graph.alloc_rule();
    for _ in 0..2 {
        let get = expr(ExprKind::Index {
            object: Box::new(expr(ExprKind::Literal(Literal::Nil))),
            index: Box::new(expr(ExprKind::Literal(Literal::Int(0)))),
            rule,
        });
        graph.block_mut(entry).steps.insert(0, Step::Eval(get));
    }
    assert_eq!(graph.freeze().validate(), Err(Invalid::Rule(rule)));
}

#[test]
fn signatures() {
    /// Validate a module with a parameterless closure whose signature has a result
    /// variable, owned by the module or else the closure, captured or not, and
    /// `params` parameter entries
    fn signed(owner_is_parent: bool, captured: bool, params: usize) -> Result<(), Invalid> {
        let graph = Graph::new();
        let top = returning(&graph);
        let lambda = closure(&graph, top);
        let owner = if owner_is_parent { top } else { lambda };
        let var = graph.alloc_var(owner, Origin::Signature, None);
        let (entry, exit) = {
            let mut func = graph.func_mut(lambda);
            if captured {
                func.captures.push(var);
            }
            func.signature = Some(Signature {
                params: vec![None; params],
                input: None,
                output: None,
                result: Some(var),
            });
            (func.entry, func.exit)
        };
        terminate(&graph, entry, Terminal::Branch(exit));
        graph.freeze().validate()
    }

    assert_eq!(signed(true, true, 0), Ok(()));
    // A variable the closure owns itself
    assert!(matches!(
        signed(false, false, 0),
        Err(Invalid::Signature(_))
    ));
    // A variable it doesn't capture
    assert!(matches!(signed(true, false, 0), Err(Invalid::Signature(_))));
    // An entry for a parameter it doesn't have
    assert!(matches!(signed(true, true, 1), Err(Invalid::Signature(_))));

    // A signature on the module function
    let graph = Graph::new();
    let top = returning(&graph);
    graph.func_mut(top).signature = Some(Signature {
        params: Vec::new(),
        input: None,
        output: None,
        result: None,
    });
    assert_eq!(graph.freeze().validate(), Err(Invalid::Signature(top)));
}
