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
        use self::{Ident, Op};
        use TokenInfo::*;

        let mut items = Vec::new();

        loop {
            match self.peek()? {
                Some(token!(Dedent)) => break Ok(items),
                Some(token!(StmtSep)) => {
                    self.advance();
                    continue;
                }
                _ => (),
            }
            items.push(match self.next()? {
                Some(token!(Op(Op::Minus), minus_span)) => {
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    match decay_ident!(self.next()?) {
                        Some(token!(Ident, span)) => ImportItem::AsIs {
                            bind: Ident::new(span),
                            delim_span: minus_span,
                            type_only: None,
                        },
                        Some(token!(At, at_span)) => {
                            self.parse_type_import_item(scope, minus_span, at_span)?
                        }
                        other => {
                            return Err(self.syntax_error(scope, other, "invalid import item"));
                        }
                    }
                }
                Some(mut token @ token!(Literal | Key)) => {
                    token = self.reinterpret_module_name(scope, token)?;
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    match decay_ident!(self.next()?) {
                        Some(token!(TokenInfo::Ident, span)) => ImportItem::Renamed {
                            item: token.span,
                            bind: Ident::new(span),
                            delim_span: token.span.after_right_char(),
                            minus_span: None,
                            type_only: None,
                        },
                        other => {
                            return Err(self.syntax_error(
                                scope,
                                other,
                                "expected identifier for renamed import",
                            ));
                        }
                    }
                }
                other => {
                    return Err(self.syntax_error(scope, other, "invalid import item"));
                }
            })
        }
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
            at_span,
            node: None,
        });
        match decay_ident!(self.next()?) {
            Some(token!(Ident, span)) => Ok(ImportItem::AsIs {
                bind: Ident::new(span),
                delim_span: minus_span,
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
            other => Err(self.syntax_error(scope, other, "invalid import item")),
        }
    }

    fn parse_import_elem_vert(&mut self, scope: &mut Scope) -> Result<ImportElement> {
        use self::{Ident, Op};
        use TokenInfo::*;

        match decay_ident!(self.peek()?) {
            Some(token!(Op(Op::Minus))) => {
                // FIXME: this needs to go back into AST
                let _minus_span = self.advance();
                self.expect(scope, &[ExpectKind::ArgSep])?;
                let (span, _) = self.parse_module_name(scope, false)?;
                Ok(ImportElement::ModuleAsIs {
                    module: span,
                    bind: Ident::new(self.module_name_first(span)),
                    insert: false,
                    type_only: None,
                })
            }
            Some(token!(Ident | Key)) => {
                let (module_span, is_key) = self.parse_module_name(scope, true)?;
                if is_key {
                    if let Some(token!(TokenInfo::Indent)) = self.peek()? {
                        self.advance();
                        let items = self.parse_import_items(scope)?;
                        self.expect(scope, &[ExpectKind::Dedent])?;
                        return Ok(ImportElement::Items {
                            module: module_span,
                            items,
                        });
                    }
                    self.expect(scope, &[ExpectKind::ArgSep])?;
                    match decay_ident!(self.next()?) {
                        Some(token!(TokenInfo::Ident, span)) => Ok(ImportElement::ModuleRenamed {
                            module: module_span,
                            bind: Ident::new(span),
                            delim_span: span.after_right_char(),
                        }),
                        other => Err(self.syntax_error(
                            scope,
                            other,
                            "expected identifier for renamed module import",
                        )),
                    }
                } else {
                    Ok(ImportElement::ModuleAsIs {
                        module: module_span,
                        bind: Ident::new(self.module_name_first(module_span)),
                        insert: false,
                        type_only: None,
                    })
                }
            }
            _ => {
                let token = self.next()?;
                Err(self.syntax_error(scope, token, "invalid import"))
            }
        }
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
                        elements: elems,
                        import_span,
                        pub_span,
                    });
                }
                _ => elems.push(self.parse_import_elem_vert(scope)?),
            }
        }
    }

    pub(super) fn parse_import(
        &mut self,
        scope: &mut Scope,
        pub_span: Option<Span>,
    ) -> Result<Import> {
        use self::{Ident, Keyword};
        use TokenInfo::*;

        let import_span = self.expect(scope, &[ExpectKind::Keyword(Keyword::Import)])?;

        let mut elems = Vec::new();

        loop {
            match self.peek()? {
                None | Some(token!(StmtSep | Dedent)) => {
                    break Ok(Import {
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
                    let (mod_span, _) = self.parse_module_name(scope, false)?;
                    ImportElement::ModuleAsIs {
                        module: mod_span,
                        bind: Ident::new(self.module_name_first(mod_span)),
                        insert: false,
                        type_only: Some(TypeOnly {
                            at_span,
                            node: None,
                        }),
                    }
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
                    return Err(self.syntax_error(scope, token, "invalid import"));
                }
            })
        }
    }
}
