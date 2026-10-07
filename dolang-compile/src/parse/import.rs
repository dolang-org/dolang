use super::{Parser, Result, Scope, stream::ExpectKind};
use crate::{
    ast::{Ident, Import, ImportElement, ImportItem, TypeOnly},
    lex::{Keyword, Op, Token, TokenInfo},
    source::Span,
};

impl Parser<'_> {
    fn reinterpret_module_name(&mut self, scope: &mut Scope, mut token: Token) -> Result<Token> {
        // Reinterpret a literal ending with `:` as a key as a special case
        if token.info == TokenInfo::Literal {
            let content = self.file.str(token.span).as_bytes();
            if !content[0].is_ascii_alphabetic()
                || content[1..content.len() - 1]
                    .iter()
                    .any(|c| !c.is_ascii_alphanumeric() && *c != b'_' && *c != b'.')
                || !content.last().unwrap() == b':'
            {
                return Err(self.syntax_error(scope, Some(token), "invalid module name"));
            }
            token.info = TokenInfo::Key;
            token.span.end -= 1;
        }
        Ok(token)
    }

    fn module_name_first(&self, span: Span) -> Span {
        if let Some((start, _)) = self.file.str(span).split_once(".") {
            (span.start..(span.start + start.len() as u32)).into()
        } else {
            span
        }
    }

    fn parse_module_name(&mut self, scope: &mut Scope, allow_key: bool) -> Result<(Span, bool)> {
        use self::Op;
        use TokenInfo::*;

        let mut result = match decay_ident!(self.next()?) {
            Some(token!(Ident, span)) => span,
            Some(token!(Key, span)) if allow_key => return Ok((span, true)),
            other => return Err(self.syntax_error(scope, other, "expected module name")),
        };

        loop {
            if let Some(token!(Op(Op::Dot))) = self.peek()? {
                self.advance();
                match self.next()? {
                    Some(token!(Ident, span)) => result = result | span,
                    Some(token!(Key, span)) if allow_key => return Ok((result | span, true)),
                    other => {
                        return Err(self.syntax_error(scope, other, "invalid module name"));
                    }
                }
            } else {
                break Ok((result, false));
            }
        }
    }

    fn parse_import_items(&mut self, scope: &mut Scope) -> Result<Vec<ImportItem>> {
        let mut items = Vec::new();

        loop {
            match self.peek()? {
                Some(token!(TokenInfo::Dedent)) => break Ok(items),
                Some(token!(TokenInfo::StmtSep)) => {
                    self.advance();
                }
                _ => self.parse_import_item_vert(scope, &mut items, false)?,
            }
        }
    }

    fn expect_import_line_end(&mut self, scope: &mut Scope, message: &'static str) -> Result<()> {
        while matches!(self.peek()?, Some(token!(TokenInfo::ArgSep))) {
            self.advance();
        }
        match self.peek()? {
            None | Some(token!(TokenInfo::StmtSep | TokenInfo::Dedent)) => Ok(()),
            token => Err(self.syntax_error(scope, token, message)),
        }
    }

    fn parse_import_item_vert(
        &mut self,
        scope: &mut Scope,
        items: &mut Vec<ImportItem>,
        packed: bool,
    ) -> Result<()> {
        use self::{Ident, Op};
        use TokenInfo::*;

        let item = match decay_ident!(self.peek()?) {
            Some(token @ token!(Op(Op::Minus), minus_span)) => {
                if packed {
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "dash import items must start a line",
                    ));
                }
                self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let item = match decay_ident!(self.next()?) {
                    Some(token!(Ident, span)) => ImportItem::AsIs {
                        bind: Ident::new(span),
                        delim_span: Some(minus_span),
                        type_only: None,
                    },
                    Some(token!(At, at_span)) => {
                        self.parse_type_import_item(scope, minus_span, at_span)?
                    }
                    Some(token @ token!(Key)) => {
                        return Err(self.syntax_error(
                            scope,
                            Some(token),
                            "renamed import items omit `-`",
                        ));
                    }
                    other => {
                        return Err(self.syntax_error(
                            scope,
                            other,
                            "expected item name after `-`",
                        ));
                    }
                };
                items.push(item);
                return self.expect_import_line_end(
                    scope,
                    "dash import items must be alone on their line",
                );
            }
            Some(token @ token!(At)) => {
                let at_span = self.advance();
                match decay_ident!(self.next()?) {
                    Some(token!(Ident, span)) => ImportItem::AsIs {
                        bind: Ident::new(span),
                        delim_span: None,
                        type_only: Some(TypeOnly {
                            at_span: Some(at_span),
                            node: None,
                        }),
                    },
                    Some(token!(Key)) => {
                        return Err(self.syntax_error(
                            scope,
                            Some(token),
                            "type-only renamed items require `- @Item: name`",
                        ));
                    }
                    other => {
                        return Err(self.syntax_error(
                            scope,
                            other,
                            "expected item name after `@`",
                        ));
                    }
                }
            }
            Some(token!(Ident, span)) => {
                self.advance();
                ImportItem::AsIs {
                    bind: Ident::new(span),
                    delim_span: None,
                    type_only: None,
                }
            }
            Some(mut token @ token!(Literal | Key)) => {
                if packed {
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "renamed import items must start a line",
                    ));
                }
                self.advance();
                token = self.reinterpret_module_name(scope, token)?;
                self.expect(scope, &[ExpectKind::ArgSep])?;
                match decay_ident!(self.next()?) {
                    Some(token!(TokenInfo::Ident, span)) => {
                        items.push(ImportItem::Renamed {
                            item: token.span,
                            bind: Ident::new(span),
                            delim_span: token.span.after_right_char(),
                            minus_span: None,
                            type_only: None,
                        });
                        return self.expect_import_line_end(
                            scope,
                            "renamed import items must be alone on their line",
                        );
                    }
                    other => {
                        return Err(self.syntax_error(
                            scope,
                            other,
                            "expected identifier for renamed import",
                        ));
                    }
                }
            }
            _ => {
                let token = self.next()?;
                return Err(self.syntax_error(scope, token, "expected imported item name"));
            }
        };
        items.push(item);
        if let Some(token!(ArgSep)) = self.peek()? {
            self.advance();
            if !matches!(self.peek()?, None | Some(token!(StmtSep | Dedent))) {
                self.parse_import_item_vert(scope, items, true)?;
            }
        }
        Ok(())
    }

    /// Parse the rest of `- @Item` or `- @Item: name`, after the `@`.
    fn parse_type_import_item(
        &mut self,
        scope: &mut Scope,
        minus_span: Span,
        at_span: Span,
    ) -> Result<ImportItem> {
        use self::Ident;
        use TokenInfo::*;

        let type_only = Some(TypeOnly {
            at_span: Some(at_span),
            node: None,
        });
        match decay_ident!(self.next()?) {
            Some(token!(Ident, span)) => Ok(ImportItem::AsIs {
                bind: Ident::new(span),
                delim_span: Some(minus_span),
                type_only,
            }),
            Some(token!(Key, item)) => {
                self.expect(scope, &[ExpectKind::ArgSep])?;
                match decay_ident!(self.next()?) {
                    Some(token!(Ident, span)) => Ok(ImportItem::Renamed {
                        item,
                        bind: Ident::new(span),
                        delim_span: item.after_right_char(),
                        minus_span: Some(minus_span),
                        type_only,
                    }),
                    other => Err(self.syntax_error(
                        scope,
                        other,
                        "expected identifier for renamed import",
                    )),
                }
            }
            other => Err(self.syntax_error(scope, other, "expected item name after `@`")),
        }
    }

    /// Parse the rest of `@module.name`, after the `@`.
    fn parse_type_import_module(
        &mut self,
        scope: &mut Scope,
        at_span: Span,
    ) -> Result<ImportElement> {
        use self::Ident;

        let (module, _) = self.parse_module_name(scope, false)?;
        Ok(ImportElement::ModuleAsIs {
            module,
            bind: Ident::new(self.module_name_first(module)),
            insert: false,
            type_only: Some(TypeOnly {
                at_span: Some(at_span),
                node: None,
            }),
        })
    }

    fn parse_import_elem_vert(
        &mut self,
        scope: &mut Scope,
        elems: &mut Vec<ImportElement>,
        packed: bool,
    ) -> Result<()> {
        use self::{Ident, Op};
        use TokenInfo::*;

        let element = match decay_ident!(self.peek()?) {
            Some(token @ token!(Op(Op::Minus))) => {
                if packed {
                    return Err(self.syntax_error(
                        scope,
                        Some(token),
                        "dash module imports must start a line",
                    ));
                }
                // FIXME: this needs to go back into AST
                let _minus_span = self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                if let Some(token!(At)) = decay_ident!(self.peek()?) {
                    let at_span = self.advance();
                    elems.push(self.parse_type_import_module(scope, at_span)?);
                } else {
                    let (span, _) = self.parse_module_name(scope, false)?;
                    elems.push(ImportElement::ModuleAsIs {
                        module: span,
                        bind: Ident::new(self.module_name_first(span)),
                        insert: false,
                        type_only: None,
                    });
                }
                return self.expect_import_line_end(
                    scope,
                    "dash module imports must be alone on their line",
                );
            }
            Some(token!(At)) => {
                let at_span = self.advance();
                self.parse_type_import_module(scope, at_span)?
            }
            Some(token @ token!(Ident | Key)) => {
                let (module_span, is_key) = self.parse_module_name(scope, true)?;
                if is_key {
                    if packed {
                        return Err(self.syntax_error(
                            scope,
                            Some(token),
                            "module imports using `:` must start a vertical continuation line",
                        ));
                    }
                    if let Some(token!(TokenInfo::Indent)) = self.peek()? {
                        self.advance();
                        let items = self.parse_import_items(scope)?;
                        self.expect(scope, &[ExpectKind::Dedent])?;
                        elems.push(ImportElement::Items {
                            module: module_span,
                            items,
                        });
                        return Ok(());
                    }
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    match decay_ident!(self.next()?) {
                        Some(token!(TokenInfo::Ident, span)) => {
                            elems.push(ImportElement::ModuleRenamed {
                                module: module_span,
                                bind: Ident::new(span),
                                delim_span: span.after_right_char(),
                                type_only: None,
                            });
                            return self.expect_import_line_end(
                                scope,
                                "renamed module imports must be alone on a vertical continuation line",
                            );
                        }
                        other => {
                            return Err(self.syntax_error(
                                scope,
                                other,
                                "expected identifier for renamed module import",
                            ));
                        }
                    }
                } else {
                    ImportElement::ModuleAsIs {
                        module: module_span,
                        bind: Ident::new(self.module_name_first(module_span)),
                        insert: false,
                        type_only: None,
                    }
                }
            }
            _ => {
                let token = self.next()?;
                return Err(self.syntax_error(scope, token, "expected module name to import"));
            }
        };
        elems.push(element);
        if let Some(token!(ArgSep)) = self.peek()? {
            self.advance();
            if !matches!(self.peek()?, None | Some(token!(StmtSep | Dedent))) {
                self.parse_import_elem_vert(scope, elems, true)?;
            }
        }
        Ok(())
    }

    fn parse_import_vert(
        &mut self,
        scope: &mut Scope,
        mut elems: Vec<ImportElement>,
        import_span: Span,
        pub_span: Option<Span>,
    ) -> Result<Import> {
        use TokenInfo::*;

        self.expect(scope, &[ExpectKind::Indent])?;

        loop {
            match self.peek()? {
                Some(token!(StmtSep)) => {
                    self.advance();
                }
                Some(token!(Dedent)) => {
                    self.advance();
                    break Ok(Import {
                        at_span: None,
                        elements: elems,
                        import_span,
                        pub_span,
                    });
                }
                _ => self.parse_import_elem_vert(scope, &mut elems, false)?,
            }
        }
    }

    pub(super) fn parse_import(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
        at_span: Option<Span>,
    ) -> Result<Import> {
        let mut import = self.parse_import_inner(scope, pub_span)?;
        import.at_span = at_span;
        if at_span.is_some() {
            for element in &mut import.elements {
                match element {
                    ImportElement::ModuleAsIs { type_only, .. }
                    | ImportElement::ModuleRenamed { type_only, .. } => {
                        type_only.get_or_insert(TypeOnly {
                            at_span: None,
                            node: None,
                        });
                    }
                    ImportElement::Items { items, .. } => {
                        for item in items {
                            let (ImportItem::AsIs { type_only, .. }
                            | ImportItem::Renamed { type_only, .. }) = item;
                            type_only.get_or_insert(TypeOnly {
                                at_span: None,
                                node: None,
                            });
                        }
                    }
                }
            }
        }
        Ok(import)
    }

    fn parse_import_inner(&mut self, scope: &mut Scope, pub_span: Option<Span>) -> Result<Import> {
        use self::{Ident, Keyword};
        use TokenInfo::*;

        let import_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Import)])?;

        let mut elems = Vec::new();

        loop {
            match self.peek()? {
                None | Some(token!(StmtSep | Dedent)) => {
                    break Ok(Import {
                        at_span: None,
                        elements: elems,
                        import_span,
                        pub_span,
                    });
                }
                Some(token!(ArgSep)) => {
                    self.advance();
                    continue;
                }
                Some(token!(Indent)) => {
                    return self.parse_import_vert(scope, elems, import_span, pub_span);
                }
                _ => (),
            }
            elems.push(match decay_ident!(self.peek()?) {
                Some(token!(At)) => {
                    let at_span = self.advance();
                    self.parse_type_import_module(scope, at_span)?
                }
                Some(token!(Ident | Key)) => {
                    let (mod_span, is_key) = self.parse_module_name(scope, true)?;
                    if is_key {
                        if let Some(token!(Indent)) = self.peek()? {
                            self.advance();
                            elems.push(ImportElement::Items {
                                module: mod_span,
                                items: self.parse_import_items(scope)?,
                            });
                            self.expect(scope, &[ExpectKind::Dedent])?;
                            break Ok(Import {
                                at_span: None,
                                elements: elems,
                                import_span,
                                pub_span,
                            });
                        }
                        self.expect(scope, &[ExpectKind::ArgSep])?;
                        match decay_ident!(self.next()?) {
                            Some(token!(TokenInfo::Ident, span)) => ImportElement::ModuleRenamed {
                                module: mod_span,
                                bind: Ident::new(span),
                                delim_span: mod_span.after_right_char(),
                                type_only: None,
                            },
                            other => {
                                return Err(self.syntax_error(
                                    scope,
                                    other,
                                    "expected identifier for renamed module import",
                                ));
                            }
                        }
                    } else {
                        ImportElement::ModuleAsIs {
                            module: mod_span,
                            bind: Ident::new(self.module_name_first(mod_span)),
                            insert: false,
                            type_only: None,
                        }
                    }
                }
                _ => {
                    let token = self.next()?;
                    return Err(self.syntax_error(scope, token, "expected module name to import"));
                }
            })
        }
    }
}
