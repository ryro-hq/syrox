use crate::ast::{
    Block, Enum, Expression, ExpressionKind, Field, Function, Ident, Input, Inputs, IntegerLiteral,
    Item, ItemKind, Literal, MatchArm, Module, Output, OutputKind, Outputs, Parameter,
    ParsedProgram, ParsedSource, ParsedSources, Path, Pattern, Primitive, PrimitiveDeclaration,
    Refinement, RefinementKind, Signature, Statement, StatementKind, StringLiteral, StringPart,
    Struct, StructField, Type, TypeAlias, TypeKind, TypeParameter, Use,
};
use crate::lexer::{Keyword, Token, TokenKind, is_keyword, lex};
use crate::{Diagnostic, MAX_DEPTH, MAX_DIAGNOSTICS, Source, SourceId, SourceSet, Span};

mod expressions;
mod items;

#[cfg(test)]
mod tests;

pub fn analyze(source: &Source) -> Result<ParsedProgram, Vec<Diagnostic>> {
    parse_source(SourceId::SINGLE, source)
}

pub fn parse_sources(sources: &SourceSet) -> Result<ParsedSources, Vec<Diagnostic>> {
    let mut parsed = Vec::with_capacity(sources.len());
    let mut errors = Vec::new();
    for (source_id, source) in sources.iter() {
        match parse_source(source_id, source) {
            Ok(program) => parsed.push(ParsedSource {
                source_id,
                domain: sources
                    .domain(source_id)
                    .expect("source iteration only yields registered sources"),
                module: sources
                    .module(source_id)
                    .expect("registered module")
                    .to_vec(),
                program,
            }),
            Err(source_errors) => {
                errors.extend(
                    source_errors
                        .into_iter()
                        .take(MAX_DIAGNOSTICS.saturating_sub(errors.len())),
                );
                if errors.len() == MAX_DIAGNOSTICS {
                    break;
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(ParsedSources {
            sources: parsed,
            input_domains: sources.input_domains().cloned().unwrap_or_default(),
            project_roots: sources.project_roots().clone(),
        })
    } else {
        errors.sort_by_key(|error| (error.span.source_id(), error.span.start(), error.span.end()));
        Err(errors)
    }
}

fn parse_source(source_id: SourceId, source: &Source) -> Result<ParsedProgram, Vec<Diagnostic>> {
    let tokens = lex(source_id, source)?;
    Parser::new(source_id, source, &tokens).parse()
}

struct Parser<'a> {
    source_id: SourceId,
    source: &'a Source,
    tokens: &'a [Token],
    at: usize,
    depth: usize,
    errors: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    fn new(source_id: SourceId, source: &'a Source, tokens: &'a [Token]) -> Self {
        Self {
            source_id,
            source,
            tokens,
            at: 0,
            depth: 0,
            errors: Vec::new(),
        }
    }

    fn parse(mut self) -> Result<ParsedProgram, Vec<Diagnostic>> {
        let items = self.parse_items(false);
        if self.errors.is_empty() {
            Ok(ParsedProgram { items })
        } else {
            Err(self.errors)
        }
    }

    fn parse_path(&mut self, message: &str) -> Option<Path> {
        let first = self.ident(message)?;
        let start = first.span;
        let mut end = first.span;
        let mut segments = vec![first];
        while self.at(TokenKind::ColonColon)
            && self
                .peek_n(1)
                .is_some_and(|token| token.kind == TokenKind::Ident)
        {
            self.advance();
            let segment = self.ident("expected path segment after `::`")?;
            end = segment.span;
            segments.push(segment);
        }
        Some(Path {
            segments,
            span: start.join(end),
        })
    }

    fn comma_idents(&mut self, close: TokenKind, allow_empty: bool) -> Option<Vec<Ident>> {
        let mut values = Vec::new();
        if self.at(close) {
            if allow_empty {
                return Some(values);
            }
            self.error(Diagnostic::error(
                "expected at least one name",
                self.peek()?.span,
            ));
            return None;
        }
        loop {
            values.push(self.ident("expected name")?);
            if self.consume(TokenKind::Comma).is_none() || self.at(close) {
                break;
            }
        }
        Some(values)
    }

    fn comma_types(&mut self, close: TokenKind, allow_empty: bool) -> Option<Vec<Type>> {
        let mut values = Vec::new();
        if self.at(close) {
            if allow_empty {
                return Some(values);
            }
            self.error(Diagnostic::error(
                "expected at least one type",
                self.peek()?.span,
            ));
            return None;
        }
        loop {
            values.push(self.parse_type()?);
            if self.consume(TokenKind::Comma).is_none() || self.at(close) {
                break;
            }
        }
        Some(values)
    }

    fn comma_expressions(&mut self, close: TokenKind) -> Option<Vec<Expression>> {
        let mut values = Vec::new();
        if self.at(close) {
            return Some(values);
        }
        loop {
            values.push(self.parse_expression()?);
            if self.consume(TokenKind::Comma).is_none() || self.at(close) {
                break;
            }
        }
        Some(values)
    }

    fn ident(&mut self, message: &str) -> Option<Ident> {
        let token = self.advance()?;
        if token.kind != TokenKind::Ident {
            self.error(Diagnostic::error(message, token.span));
            return None;
        }
        Some(Ident {
            text: self.text(token.span).to_owned(),
            span: token.span,
        })
    }

    fn enter(&mut self, span: Span) -> bool {
        if self.depth == MAX_DEPTH {
            self.error(Diagnostic::error(
                format!("syntax nesting exceeds the limit of {MAX_DEPTH}"),
                span,
            ));
            false
        } else {
            self.depth += 1;
            true
        }
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn recover_item(&mut self, nested: bool) {
        let mut depth = 0usize;
        while let Some(token) = self.peek() {
            match token.kind {
                TokenKind::LeftBrace | TokenKind::LeftBracket | TokenKind::LeftParen => depth += 1,
                TokenKind::RightBrace if depth == 0 => {
                    if !nested {
                        self.advance();
                    }
                    return;
                }
                TokenKind::RightBrace | TokenKind::RightBracket | TokenKind::RightParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        self.advance();
                        return;
                    }
                }
                TokenKind::Semicolon if depth == 0 => {
                    self.advance();
                    return;
                }
                _ => {}
            }
            self.advance();
        }
    }

    fn error(&mut self, diagnostic: Diagnostic) {
        if self.errors.len() < MAX_DIAGNOSTICS {
            self.errors.push(diagnostic);
        }
    }

    fn expect(&mut self, kind: TokenKind, message: &str) -> Option<Token> {
        let Some(token) = self.advance() else {
            self.error(Diagnostic::error(message, self.eof_span()));
            return None;
        };
        if token.kind == kind {
            Some(token)
        } else {
            self.error(Diagnostic::error(message, token.span));
            None
        }
    }

    fn expect_keyword(&mut self, keyword: Keyword, message: &str) -> Option<Token> {
        self.expect(TokenKind::Keyword(keyword), message)
    }

    fn consume(&mut self, kind: TokenKind) -> Option<Token> {
        if self.at(kind) { self.advance() } else { None }
    }

    fn consume_keyword(&mut self, keyword: Keyword) -> Option<Token> {
        self.consume(TokenKind::Keyword(keyword))
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.peek().is_some_and(|token| token.kind == kind)
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        self.at(TokenKind::Keyword(keyword))
    }

    fn peek(&self) -> Option<Token> {
        self.peek_n(0)
    }

    fn peek_n(&self, offset: usize) -> Option<Token> {
        self.tokens.get(self.at + offset).copied()
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.peek()?;
        self.at += 1;
        Some(token)
    }

    fn text(&self, span: Span) -> &str {
        &self.source.text()[span.range()]
    }

    fn subspan(outer: Span, start: usize, end: usize) -> Span {
        Span::new(
            outer.source_id(),
            outer
                .start()
                .saturating_add(u32::try_from(start).unwrap_or(u32::MAX)),
            outer
                .start()
                .saturating_add(u32::try_from(end).unwrap_or(u32::MAX)),
        )
    }

    fn eof_span(&self) -> Span {
        let end = u32::try_from(self.source.text().len()).unwrap_or(u32::MAX);
        Span::new(self.source_id, end, end)
    }
}
