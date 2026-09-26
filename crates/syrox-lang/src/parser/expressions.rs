use super::{
    Diagnostic, Expression, ExpressionKind, Ident, IntegerLiteral, Keyword, Literal, MatchArm,
    Parser, Path, Pattern, Span, StringLiteral, StringPart, StructField, TokenKind, Type,
    is_keyword,
};

impl Parser<'_> {
    pub(super) fn parse_expression(&mut self) -> Option<Expression> {
        self.parse_concat(true)
    }

    fn parse_concat(&mut self, allow_struct: bool) -> Option<Expression> {
        let first = self.parse_postfix(allow_struct)?;
        if !self.at(TokenKind::PlusPlus) {
            return Some(first);
        }
        let start = first.span;
        let mut expressions = vec![first];
        while self.consume(TokenKind::PlusPlus).is_some() {
            expressions.push(self.parse_postfix(allow_struct)?);
        }
        let span = start.join(expressions.last()?.span);
        Some(Expression {
            kind: ExpressionKind::Concat(expressions),
            span,
        })
    }

    fn parse_postfix(&mut self, allow_struct: bool) -> Option<Expression> {
        let expression = self.parse_primary(allow_struct)?;
        let mut fields = Vec::new();
        while self.consume(TokenKind::Dot).is_some() {
            fields.push(self.ident("expected field name after `.`")?);
        }
        if fields.is_empty() {
            return Some(expression);
        }
        let span = expression.span.join(fields.last()?.span);
        Some(Expression {
            kind: ExpressionKind::Field {
                value: Box::new(expression),
                fields,
            },
            span,
        })
    }

    fn parse_primary(&mut self, allow_struct: bool) -> Option<Expression> {
        let token = self.peek()?;
        match token.kind {
            TokenKind::Integer => self.integer_literal().map(|literal| Expression {
                span: literal.span,
                kind: ExpressionKind::Integer(literal),
            }),
            TokenKind::String => self.string_literal().map(|literal| Expression {
                span: literal.span,
                kind: ExpressionKind::String(literal),
            }),
            TokenKind::Ident => self.parse_path_expression(allow_struct),
            TokenKind::LeftBracket => self.parse_list(),
            TokenKind::LeftParen => self.parse_group(),
            TokenKind::Keyword(Keyword::Erase) => self.parse_erase(),
            TokenKind::Keyword(Keyword::Match) => self.parse_match(),
            _ => {
                self.error(Diagnostic::error("expected expression", token.span));
                None
            }
        }
    }

    fn parse_path_expression(&mut self, allow_struct: bool) -> Option<Expression> {
        let path = self.parse_path("expected path")?;
        let start = path.span;
        if let Some(open) = self.consume(TokenKind::LeftParen) {
            if !self.enter(open.span) {
                return None;
            }
            let arguments = self.comma_expressions(TokenKind::RightParen);
            let close = self.expect(TokenKind::RightParen, "expected `)` after call arguments");
            self.leave();
            let close = close?;
            return Some(Expression {
                kind: ExpressionKind::Call {
                    callee: path,
                    arguments: arguments?,
                },
                span: start.join(close.span),
            });
        }
        let type_arguments = if allow_struct && self.at(TokenKind::LeftAngle) {
            let open = self.advance()?;
            if !self.enter(open.span) {
                return None;
            }
            let arguments = self.comma_types(TokenKind::RightAngle, false);
            let close = self.expect(TokenKind::RightAngle, "expected `>` after type arguments");
            self.leave();
            close?;
            arguments?
        } else {
            Vec::new()
        };
        if allow_struct && self.at(TokenKind::LeftBrace) {
            return self.parse_struct_literal(path, type_arguments);
        }
        if !type_arguments.is_empty() {
            self.error(Diagnostic::error(
                "generic arguments in an expression require a struct literal",
                start,
            ));
            return None;
        }
        Some(Expression {
            kind: ExpressionKind::Path(path),
            span: start,
        })
    }

    fn parse_struct_literal(
        &mut self,
        path: Path,
        type_arguments: Vec<Type>,
    ) -> Option<Expression> {
        let start = path.span;
        let open = self.expect(TokenKind::LeftBrace, "expected `{` in struct literal")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut fields = Vec::new();
        while self.peek().is_some() && !self.at(TokenKind::RightBrace) {
            let field_start = self.peek()?.span;
            let name = self.ident("expected struct field name")?;
            self.expect(TokenKind::Equal, "expected `=` after struct field name")?;
            let value = self.parse_expression()?;
            let end = self.expect(TokenKind::Semicolon, "expected `;` after struct field")?;
            fields.push(StructField {
                name,
                value,
                span: field_start.join(end.span),
            });
        }
        let close = self.expect(TokenKind::RightBrace, "expected `}` after struct literal");
        self.leave();
        let close = close?;
        Some(Expression {
            kind: ExpressionKind::Struct {
                path,
                type_arguments,
                fields,
            },
            span: start.join(close.span),
        })
    }

    fn parse_list(&mut self) -> Option<Expression> {
        let open = self.expect(TokenKind::LeftBracket, "expected `[")?;
        if !self.enter(open.span) {
            return None;
        }
        let values = self.comma_expressions(TokenKind::RightBracket);
        let close = self.expect(TokenKind::RightBracket, "expected `]` after list");
        self.leave();
        let close = close?;
        Some(Expression {
            kind: ExpressionKind::List(values?),
            span: open.span.join(close.span),
        })
    }

    fn parse_group(&mut self) -> Option<Expression> {
        let open = self.expect(TokenKind::LeftParen, "expected `(`")?;
        if !self.enter(open.span) {
            return None;
        }
        let value = self.parse_expression();
        let close = self.expect(TokenKind::RightParen, "expected `)` after expression");
        self.leave();
        let (value, close) = (value?, close?);
        Some(Expression {
            kind: ExpressionKind::Group(Box::new(value)),
            span: open.span.join(close.span),
        })
    }

    fn parse_erase(&mut self) -> Option<Expression> {
        let start = self
            .expect_keyword(Keyword::Erase, "expected `erase`")?
            .span;
        let open = self.expect(TokenKind::LeftAngle, "expected `<` after `erase`")?;
        if !self.enter(open.span) {
            return None;
        }
        let ty = self.parse_type()?;
        let close = self.expect(TokenKind::RightAngle, "expected `>` after erased type");
        self.leave();
        close?;
        let open = self.expect(TokenKind::LeftParen, "expected `(` after erased type")?;
        if !self.enter(open.span) {
            return None;
        }
        let value = self.parse_expression();
        let close = self.expect(TokenKind::RightParen, "expected `)` after erased value");
        self.leave();
        let (value, close) = (value?, close?);
        Some(Expression {
            kind: ExpressionKind::Erase {
                ty,
                value: Box::new(value),
            },
            span: start.join(close.span),
        })
    }

    fn parse_match(&mut self) -> Option<Expression> {
        let start = self
            .expect_keyword(Keyword::Match, "expected `match`")?
            .span;
        if !self.enter(start) {
            return None;
        }
        // A bare `{` after the scrutinee starts the match body, not a struct literal.
        let value = self.parse_concat(false)?;
        let open = self.expect(TokenKind::LeftBrace, "expected `{` after match value")?;
        if !self.enter(open.span) {
            return None;
        }
        let mut arms = Vec::new();
        while self.peek().is_some() && !self.at(TokenKind::RightBrace) {
            let arm_start = self.peek()?.span;
            let pattern = self.parse_pattern()?;
            self.expect(TokenKind::FatArrow, "expected `=>` after match pattern")?;
            let arm_value = self.parse_expression()?;
            let arm_end = arm_value.span;
            arms.push(MatchArm {
                pattern,
                value: arm_value,
                span: arm_start.join(arm_end),
            });
            if self.consume(TokenKind::Comma).is_none() {
                break;
            }
        }
        if arms.is_empty() {
            self.error(Diagnostic::error(
                "match requires at least one arm",
                open.span,
            ));
        }
        let close = self.expect(TokenKind::RightBrace, "expected `}` after match arms");
        self.leave();
        let close = close?;
        let expression = Expression {
            kind: ExpressionKind::Match {
                value: Box::new(value),
                arms,
            },
            span: start.join(close.span),
        };
        self.leave();
        Some(expression)
    }

    fn parse_pattern(&mut self) -> Option<Pattern> {
        if self
            .peek()
            .is_some_and(|token| token.kind == TokenKind::Ident && self.text(token.span) == "_")
        {
            return self.advance().map(|token| Pattern::Wildcard(token.span));
        }
        self.parse_path("expected match pattern").map(Pattern::Path)
    }

    pub(super) fn parse_literal(&mut self) -> Option<Literal> {
        match self.peek()?.kind {
            TokenKind::Integer => self.integer_literal().map(Literal::Integer),
            TokenKind::String => self.string_literal().map(Literal::String),
            _ => {
                self.error(Diagnostic::error(
                    "expected integer or string literal",
                    self.peek()?.span,
                ));
                None
            }
        }
    }

    pub(super) fn integer_literal(&mut self) -> Option<IntegerLiteral> {
        let token = self.expect(TokenKind::Integer, "expected integer")?;
        let source = self.text(token.span).to_owned();
        let Ok(value) = source.parse::<i64>() else {
            self.error(Diagnostic::error("integer literal exceeds i64", token.span));
            return None;
        };
        Some(IntegerLiteral {
            source,
            value,
            span: token.span,
        })
    }

    pub(super) fn string_literal(&mut self) -> Option<StringLiteral> {
        let token = self.expect(TokenKind::String, "expected string literal")?;
        let raw = self.text(token.span).to_owned();
        let bytes = raw.as_bytes();
        if bytes.len() < 2 || bytes.last() != Some(&b'"') {
            return None;
        }
        let mut parts = Vec::new();
        let mut at = 1;
        let mut text_start = at;
        while at + 1 < bytes.len() {
            if bytes[at] == b'\\' {
                at += 2;
                continue;
            }
            if bytes[at] == b'$' && bytes.get(at + 1) == Some(&b'{') {
                if text_start < at {
                    parts.push(StringPart::Text {
                        source: raw[text_start..at].to_owned(),
                        span: Self::subspan(token.span, text_start, at),
                    });
                }
                let interpolation_start = at;
                at += 2;
                let path_start = at;
                while at + 1 < bytes.len() && bytes[at] != b'}' {
                    at += 1;
                }
                if at + 1 >= bytes.len() {
                    self.error(Diagnostic::error(
                        "unterminated string interpolation",
                        Self::subspan(token.span, interpolation_start, at),
                    ));
                    return None;
                }
                let path = self.interpolation_path(token.span, &raw, path_start, at)?;
                at += 1;
                parts.push(StringPart::Interpolation {
                    path,
                    span: Self::subspan(token.span, interpolation_start, at),
                });
                text_start = at;
            } else {
                at += 1;
            }
        }
        if text_start < bytes.len() - 1 {
            parts.push(StringPart::Text {
                source: raw[text_start..bytes.len() - 1].to_owned(),
                span: Self::subspan(token.span, text_start, bytes.len() - 1),
            });
        }
        Some(StringLiteral {
            source: raw,
            parts,
            span: token.span,
        })
    }

    fn interpolation_path(
        &mut self,
        token_span: Span,
        raw: &str,
        start: usize,
        end: usize,
    ) -> Option<Path> {
        let bytes = raw.as_bytes();
        let mut segments = Vec::new();
        let mut at = start;
        while at < end {
            let segment_start = at;
            if !bytes[at].is_ascii_alphabetic() && bytes[at] != b'_' {
                self.error(Diagnostic::error(
                    "interpolation must contain a local name and optional fields",
                    Self::subspan(token_span, start, end),
                ));
                return None;
            }
            at += 1;
            while at < end && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            let span = Self::subspan(token_span, segment_start, at);
            let text = &raw[segment_start..at];
            if is_keyword(text) {
                self.error(Diagnostic::error(
                    "interpolation names cannot be reserved keywords",
                    span,
                ));
                return None;
            }
            segments.push(Ident {
                text: text.to_owned(),
                span,
            });
            if at == end {
                break;
            }
            if bytes[at] != b'.' {
                self.error(Diagnostic::error(
                    "interpolation fields must be separated by `.`",
                    Self::subspan(token_span, start, end),
                ));
                return None;
            }
            at += 1;
        }
        if segments.is_empty() || raw.as_bytes().get(end.saturating_sub(1)) == Some(&b'.') {
            self.error(Diagnostic::error(
                "interpolation requires a name after `.`",
                Self::subspan(token_span, start, end),
            ));
            return None;
        }
        let span = Self::subspan(token_span, start, end);
        Some(Path { segments, span })
    }
}
