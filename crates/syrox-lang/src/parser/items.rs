use super::{
    Block, Diagnostic, Enum, Field, Function, Input, Inputs, Item, ItemKind, Keyword,
    MAX_DIAGNOSTICS, Module, Output, OutputKind, Outputs, Parameter, Parser, Primitive,
    PrimitiveDeclaration, Refinement, RefinementKind, Signature, Span, Statement, StatementKind,
    Struct, Token, TokenKind, Type, TypeAlias, TypeKind, TypeParameter, Use,
};

impl Parser<'_> {
    pub(super) fn parse_items(&mut self, nested: bool) -> Vec<Item> {
        let mut items = Vec::new();
        while self.peek().is_some() && !(nested && self.at(TokenKind::RightBrace)) {
            let before = self.at;
            if let Some(item) = self.parse_item() {
                items.push(item);
            } else {
                self.recover_item(nested);
            }
            if self.at == before {
                self.advance();
            }
            if self.errors.len() == MAX_DIAGNOSTICS {
                break;
            }
        }
        items
    }

    pub(super) fn parse_item(&mut self) -> Option<Item> {
        let start_at = self.at;
        let start_depth = self.depth;
        let item = self.parse_item_inner();
        self.depth = start_depth;
        if item.is_none() {
            self.at = start_at;
        }
        item
    }

    fn parse_item_inner(&mut self) -> Option<Item> {
        let token = self.advance()?;
        let public = token.kind == TokenKind::Keyword(Keyword::Pub);
        let declaration = if public { self.advance()? } else { token };
        let TokenKind::Keyword(keyword) = declaration.kind else {
            self.error(
                Diagnostic::error("expected a declaration", token.span).with_note(
                    "declarations start with mod, use, inputs, outputs, type, struct, opaque, enum, resource, value or fn",
                ),
            );
            return None;
        };
        if public
            && !matches!(
                keyword,
                Keyword::Fn
                    | Keyword::Struct
                    | Keyword::Type
                    | Keyword::Enum
                    | Keyword::Value
                    | Keyword::Resource
                    | Keyword::Opaque
                    | Keyword::Use
            )
        {
            self.error(Diagnostic::error(
                "`pub` requires a type, value, function or import declaration",
                declaration.span,
            ));
            return None;
        }
        let start = token.span;
        let (kind, end) = match keyword {
            Keyword::Mod => self.parse_module(start)?,
            Keyword::Use => self.parse_use(start)?,
            Keyword::Inputs => self.parse_inputs(start)?,
            Keyword::Outputs => self.parse_outputs(start)?,
            Keyword::Type => self.parse_type_alias(start)?,
            Keyword::Struct => self.parse_struct(start, false)?,
            Keyword::Opaque => {
                self.expect_keyword(Keyword::Struct, "expected `struct` after `opaque`")?;
                self.parse_struct(start, true)?
            }
            Keyword::Enum => self.parse_enum(start)?,
            Keyword::Resource => self.parse_primitive_declaration(start, true)?,
            Keyword::Value => self.parse_primitive_declaration(start, false)?,
            Keyword::Fn => self.parse_function(start)?,
            _ => {
                self.error(Diagnostic::error("expected a declaration", start));
                return None;
            }
        };
        Some(Item {
            kind,
            public,
            span: start.join(end),
        })
    }

    fn parse_module(&mut self, start: Span) -> Option<(ItemKind, Span)> {
        let path = self.parse_path("expected module path")?;
        let open = self.expect(TokenKind::LeftBrace, "expected `{` after module path")?;
        if !self.enter(open.span) {
            return None;
        }
        let items = self.parse_items(true);
        let close = self.expect(TokenKind::RightBrace, "expected `}` after module body");
        self.leave();
        let close = close?;
        let _ = start;
        Some((ItemKind::Module(Module { path, items }), close.span))
    }

    fn parse_use(&mut self, _start: Span) -> Option<(ItemKind, Span)> {
        let path = self.parse_path("expected import path")?;
        let names = if self.at(TokenKind::ColonColon)
            && self
                .peek_n(1)
                .is_some_and(|token| token.kind == TokenKind::LeftBrace)
        {
            self.advance();
            let open = self.advance()?;
            if !self.enter(open.span) {
                return None;
            }
            let names = self.comma_idents(TokenKind::RightBrace, false);
            let close = self.expect(TokenKind::RightBrace, "expected `}` after imported names");
            self.leave();
            close?;
            Some(names?)
        } else {
            None
        };
        let end = self.expect(TokenKind::Semicolon, "expected `;` after import")?;
        Some((ItemKind::Use(Use { path, names }), end.span))
    }

    fn parse_inputs(&mut self, _start: Span) -> Option<(ItemKind, Span)> {
        let open = self.expect(TokenKind::LeftBrace, "expected `{` after `inputs`")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut entries = Vec::new();
        while self.peek().is_some() && !self.at(TokenKind::RightBrace) {
            let name = self.ident("expected input name")?;
            self.expect(TokenKind::Equal, "expected `=` after input name")?;
            let value = self.string_literal()?;
            let end = self.expect(TokenKind::Semicolon, "expected `;` after input")?;
            entries.push(Input {
                span: name.span.join(end.span),
                name,
                value,
            });
        }
        let close = self.expect(TokenKind::RightBrace, "expected `}` after inputs");
        self.leave();
        let close = close?;
        Some((ItemKind::Inputs(Inputs { entries }), close.span))
    }

    fn parse_outputs(&mut self, _start: Span) -> Option<(ItemKind, Span)> {
        let open = self.expect(TokenKind::LeftBrace, "expected `{` after `outputs`")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut entries = Vec::new();
        while self.peek().is_some() && !self.at(TokenKind::RightBrace) {
            let entry_start = self.peek()?.span;
            let kind = if self.consume_keyword(Keyword::Type).is_some() {
                let name = self.ident("expected exported type name")?;
                self.expect(TokenKind::Equal, "expected `=` after exported type name")?;
                let ty = self.parse_type()?;
                OutputKind::Type { name, ty }
            } else {
                let name = self.ident("expected output name")?;
                self.expect(TokenKind::Colon, "expected `:` after output name")?;
                if self.at_keyword(Keyword::Fn) {
                    let signature = self.parse_signature()?;
                    self.expect(TokenKind::Equal, "expected `=` after function signature")?;
                    let function = self.parse_path("expected exported function path")?;
                    OutputKind::Function {
                        name,
                        signature,
                        function,
                    }
                } else {
                    let ty = self.parse_type()?;
                    self.expect(TokenKind::Equal, "expected `=` after output type")?;
                    let value = self.parse_expression()?;
                    OutputKind::Value { name, ty, value }
                }
            };
            let end = self.expect(TokenKind::Semicolon, "expected `;` after output")?;
            entries.push(Output {
                kind,
                span: entry_start.join(end.span),
            });
        }
        let close = self.expect(TokenKind::RightBrace, "expected `}` after outputs");
        self.leave();
        let close = close?;
        Some((ItemKind::Outputs(Outputs { entries }), close.span))
    }

    fn parse_signature(&mut self) -> Option<Signature> {
        let start = self
            .expect_keyword(Keyword::Fn, "expected function signature")?
            .span;
        let open = self.expect(TokenKind::LeftParen, "expected `(` in function signature")?;
        if !self.enter(open.span) {
            return None;
        }
        let parameters = self.comma_types(TokenKind::RightParen, true);
        let close = self.expect(
            TokenKind::RightParen,
            "expected `)` after signature parameters",
        );
        self.leave();
        close?;
        self.expect(TokenKind::Arrow, "expected `->` in function signature")?;
        let result = self.parse_type()?;
        Some(Signature {
            span: start.join(result.span),
            parameters: parameters?,
            result,
        })
    }

    fn parse_type_alias(&mut self, _start: Span) -> Option<(ItemKind, Span)> {
        let name = self.ident("expected type alias name")?;
        let type_parameters = if self.at(TokenKind::LeftAngle) {
            self.parse_type_parameters()?
        } else {
            Vec::new()
        };
        self.expect(TokenKind::Equal, "expected `=` after type alias name")?;
        let ty = self.parse_type()?;
        let end = self.expect(TokenKind::Semicolon, "expected `;` after type alias")?;
        Some((
            ItemKind::TypeAlias(TypeAlias {
                name,
                type_parameters,
                ty,
            }),
            end.span,
        ))
    }

    fn parse_struct(&mut self, _start: Span, opaque: bool) -> Option<(ItemKind, Span)> {
        let name = self.ident("expected struct name")?;
        let type_parameters = if self.at(TokenKind::LeftAngle) {
            self.parse_type_parameters()?
        } else {
            Vec::new()
        };
        let scopes = if self.consume_keyword(Keyword::In).is_some() {
            let open = self.expect(TokenKind::LeftBracket, "expected `[` after `in`")?;
            if !self.enter(open.span) {
                return None;
            }
            let scopes = self.comma_idents(TokenKind::RightBracket, true);
            let close = self.expect(TokenKind::RightBracket, "expected `]` after scopes");
            self.leave();
            close?;
            scopes?
        } else {
            Vec::new()
        };
        let open = self.expect(TokenKind::LeftBrace, "expected `{` before struct fields")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut fields = Vec::new();
        while self.peek().is_some() && !self.at(TokenKind::RightBrace) {
            let field_start = self.peek()?.span;
            let field_name = self.ident("expected field name")?;
            self.expect(TokenKind::Colon, "expected `:` after field name")?;
            let ty = self.parse_type()?;
            let default = if self.consume(TokenKind::Equal).is_some() {
                Some(self.parse_expression()?)
            } else {
                None
            };
            let end = self.expect(TokenKind::Semicolon, "expected `;` after field")?;
            fields.push(Field {
                name: field_name,
                ty,
                default,
                span: field_start.join(end.span),
            });
        }
        let close = self.expect(TokenKind::RightBrace, "expected `}` after struct fields");
        self.leave();
        let close = close?;
        Some((
            ItemKind::Struct(Struct {
                name,
                opaque,
                type_parameters,
                scopes,
                fields,
            }),
            close.span,
        ))
    }

    fn parse_type_parameters(&mut self) -> Option<Vec<TypeParameter>> {
        let open = self.expect(TokenKind::LeftAngle, "expected `<`")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut parameters = Vec::new();
        loop {
            let start = self.peek()?.span;
            let owner = self.consume_keyword(Keyword::Owner).is_some();
            let name = self.ident("expected type parameter")?;
            parameters.push(TypeParameter {
                span: start.join(name.span),
                name,
                owner,
            });
            if self.consume(TokenKind::Comma).is_none() {
                break;
            }
            if self.at(TokenKind::RightAngle) {
                self.error(Diagnostic::error(
                    "type parameter lists do not allow a trailing comma",
                    self.peek()?.span,
                ));
                break;
            }
        }
        let close = self.expect(TokenKind::RightAngle, "expected `>` after type parameters");
        self.leave();
        close?;
        Some(parameters)
    }

    fn parse_enum(&mut self, _start: Span) -> Option<(ItemKind, Span)> {
        let name = self.ident("expected enum name")?;
        let type_parameters = if self.at(TokenKind::LeftAngle) {
            self.parse_type_parameters()?
        } else {
            Vec::new()
        };
        let open = self.expect(TokenKind::LeftBrace, "expected `{` before enum variants")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut variants = Vec::new();
        while !self.at(TokenKind::RightBrace) {
            let name = self.ident("expected enum variant name")?;
            let payload = if let Some(open) = self.consume(TokenKind::LeftParen) {
                if !self.enter(open.span) {
                    return None;
                }
                let payload = if self.at(TokenKind::RightParen) {
                    Some(Vec::new())
                } else {
                    self.comma_types(TokenKind::RightParen, true)
                };
                let close =
                    self.expect(TokenKind::RightParen, "expected `)` after variant payload");
                self.leave();
                close?;
                payload?
            } else {
                Vec::new()
            };
            variants.push(crate::ast::EnumVariant { name, payload });
            if self.consume(TokenKind::Comma).is_none() {
                break;
            }
        }
        let close = self.expect(TokenKind::RightBrace, "expected `}` after enum variants");
        self.leave();
        let close = close?;
        Some((
            ItemKind::Enum(Enum {
                name,
                type_parameters,
                variants,
            }),
            close.span,
        ))
    }

    fn parse_primitive_declaration(
        &mut self,
        _start: Span,
        resource: bool,
    ) -> Option<(ItemKind, Span)> {
        let name = self.ident("expected declaration name")?;
        let open = self.expect(TokenKind::LeftParen, "expected `(` after declaration name")?;
        if !self.enter(open.span) {
            return None;
        }
        let primitive = match self.advance()? {
            Token {
                kind: TokenKind::Keyword(Keyword::Int),
                ..
            } => Primitive::Int,
            Token {
                kind: TokenKind::Keyword(Keyword::Str),
                ..
            } => Primitive::Str,
            token => {
                self.error(Diagnostic::error(
                    "expected primitive `int` or `str`",
                    token.span,
                ));
                self.leave();
                return None;
            }
        };
        let close = self.expect(TokenKind::RightParen, "expected `)` after primitive");
        self.leave();
        close?;
        let scope = if self.consume_keyword(Keyword::In).is_some() {
            Some(self.ident("expected scope name after `in`")?)
        } else {
            None
        };
        let mut refinements = Vec::new();
        if self.consume_keyword(Keyword::Where).is_some() {
            refinements.push(self.parse_refinement()?);
            while self.consume(TokenKind::Comma).is_some() {
                refinements.push(self.parse_refinement()?);
            }
        }
        let end = self.expect(TokenKind::Semicolon, "expected `;` after declaration")?;
        let declaration = PrimitiveDeclaration {
            name,
            primitive,
            scope,
            refinements,
        };
        let kind = if resource {
            ItemKind::Resource(declaration)
        } else {
            ItemKind::Value(declaration)
        };
        Some((kind, end.span))
    }

    fn parse_refinement(&mut self) -> Option<Refinement> {
        let start = self.peek()?.span;
        if self.at(TokenKind::Integer) {
            let range_start = self.integer_literal()?;
            let inclusive = match self.advance()? {
                Token {
                    kind: TokenKind::DotDot,
                    ..
                } => false,
                Token {
                    kind: TokenKind::DotDotEqual,
                    ..
                } => true,
                token => {
                    self.error(Diagnostic::error(
                        "expected `..` or `..=` in range",
                        token.span,
                    ));
                    return None;
                }
            };
            let end = self.integer_literal()?;
            return Some(Refinement {
                span: start.join(end.span),
                kind: RefinementKind::Range {
                    start: range_start,
                    end,
                    inclusive,
                },
            });
        }
        if self.consume_keyword(Keyword::In).is_some() {
            let open = self.expect(TokenKind::LeftBracket, "expected `[` after `in`")?;
            if !self.enter(open.span) {
                return None;
            }
            let mut values = Vec::new();
            loop {
                values.push(self.parse_literal()?);
                if self.consume(TokenKind::Comma).is_none() || self.at(TokenKind::RightBracket) {
                    break;
                }
            }
            let close = self.expect(TokenKind::RightBracket, "expected `]` after refinement set");
            self.leave();
            let close = close?;
            return Some(Refinement {
                kind: RefinementKind::Set(values),
                span: start.join(close.span),
            });
        }
        let path = self.parse_path("expected refinement")?;
        self.expect(
            TokenKind::LeftParen,
            "expected `(` after refinement predicate",
        )?;
        self.expect_keyword(
            Keyword::SelfValue,
            "expected `self` in refinement predicate",
        )?;
        let close = self.expect(TokenKind::RightParen, "expected `)` after `self`")?;
        Some(Refinement {
            kind: RefinementKind::Predicate(path),
            span: start.join(close.span),
        })
    }

    fn parse_function(&mut self, _start: Span) -> Option<(ItemKind, Span)> {
        let name = self.ident("expected function name")?;
        let type_parameters = if self.at(TokenKind::LeftAngle) {
            self.parse_type_parameters()?
        } else {
            Vec::new()
        };
        let open = self.expect(TokenKind::LeftParen, "expected `(` after function name")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut parameters = Vec::new();
        if !self.at(TokenKind::RightParen) {
            loop {
                let parameter_start = self.peek()?.span;
                let parameter_name = self.ident("expected parameter name")?;
                self.expect(TokenKind::Colon, "expected `:` after parameter name")?;
                let ty = self.parse_type()?;
                parameters.push(Parameter {
                    span: parameter_start.join(ty.span),
                    name: parameter_name,
                    ty,
                });
                if self.consume(TokenKind::Comma).is_none() || self.at(TokenKind::RightParen) {
                    break;
                }
            }
        }
        let close = self.expect(TokenKind::RightParen, "expected `)` after parameters");
        self.leave();
        close?;
        let result = if self.consume(TokenKind::Arrow).is_some() {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        let end = body.span;
        Some((
            ItemKind::Function(Function {
                name,
                type_parameters,
                parameters,
                result,
                body,
            }),
            end,
        ))
    }

    pub(super) fn parse_block(&mut self) -> Option<Block> {
        let open = self.expect(TokenKind::LeftBrace, "expected function body")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut statements = Vec::new();
        let mut tail = None;
        while self.peek().is_some() && !self.at(TokenKind::RightBrace) {
            let start = self.peek()?.span;
            if self.consume_keyword(Keyword::Let).is_some() {
                let name = self.ident("expected name after `let`")?;
                self.expect(TokenKind::Equal, "expected `=` in let statement")?;
                let value = self.parse_expression()?;
                let end = self.expect(TokenKind::Semicolon, "expected `;` after let statement")?;
                statements.push(Statement {
                    kind: StatementKind::Let { name, value },
                    span: start.join(end.span),
                });
                continue;
            }
            let expression = self.parse_expression()?;
            if let Some(end) = self.consume(TokenKind::Semicolon) {
                statements.push(Statement {
                    kind: StatementKind::Expression(expression),
                    span: start.join(end.span),
                });
            } else {
                tail = Some(Box::new(expression));
                break;
            }
        }
        let close = self.expect(TokenKind::RightBrace, "expected `}` after function body");
        self.leave();
        let close = close?;
        Some(Block {
            statements,
            tail,
            span: open.span.join(close.span),
        })
    }

    pub(super) fn parse_type(&mut self) -> Option<Type> {
        let once = self.consume_keyword(Keyword::Once);
        if once.is_some() && !self.at_keyword(Keyword::Fn) {
            self.expect_keyword(Keyword::Fn, "expected `fn` after `once`")?;
        }
        if let Some(start) = self.consume_keyword(Keyword::Fn) {
            let open = self.expect(TokenKind::LeftParen, "expected `(` after `fn` type")?;
            if !self.enter(open.span) {
                return None;
            }
            let parameters = if self.at(TokenKind::RightParen) {
                Some(Vec::new())
            } else {
                self.comma_types(TokenKind::RightParen, false)
            };
            let close = self.expect(
                TokenKind::RightParen,
                "expected `)` after function parameters",
            );
            self.leave();
            close?;
            self.expect(TokenKind::Arrow, "expected `->` after function parameters")?;
            let result = self.parse_type()?;
            return Some(Type {
                span: once
                    .map_or(start.span, |token| token.span)
                    .join(result.span),
                kind: TypeKind::Function {
                    once: once.is_some(),
                    parameters: parameters?,
                    result: Box::new(result),
                },
            });
        }
        if let Some(open) = self.consume(TokenKind::LeftBracket) {
            if !self.enter(open.span) {
                return None;
            }
            let element = self.parse_type()?;
            let close = self.expect(TokenKind::RightBracket, "expected `]` after list type");
            self.leave();
            let close = close?;
            return Some(Type {
                kind: TypeKind::List(Box::new(element)),
                span: open.span.join(close.span),
            });
        }
        let path = self.parse_path("expected type")?;
        let start = path.span;
        let (arguments, end) = if let Some(open) = self.consume(TokenKind::LeftAngle) {
            if !self.enter(open.span) {
                return None;
            }
            let arguments = self.comma_types(TokenKind::RightAngle, false);
            let close = self.expect(TokenKind::RightAngle, "expected `>` after type arguments");
            self.leave();
            let close = close?;
            (arguments?, close.span)
        } else {
            (Vec::new(), path.span)
        };
        Some(Type {
            kind: TypeKind::Named { path, arguments },
            span: start.join(end),
        })
    }
}
