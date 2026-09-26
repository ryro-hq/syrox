use crate::{Diagnostic, MAX_DIAGNOSTICS, MAX_TOKENS, Source, SourceId, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Ident,
    String,
    Integer,
    Keyword(Keyword),
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    LeftParen,
    RightParen,
    LeftAngle,
    RightAngle,
    Colon,
    ColonColon,
    Comma,
    Dot,
    DotDot,
    DotDotEqual,
    Equal,
    FatArrow,
    Arrow,
    PlusPlus,
    Semicolon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keyword {
    Enum,
    Erase,
    Fn,
    In,
    Inputs,
    Int,
    Let,
    Match,
    Mod,
    Opaque,
    Outputs,
    Owner,
    Resource,
    SelfValue,
    Str,
    Struct,
    Type,
    Use,
    Value,
    Where,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn lex(source_id: SourceId, source: &Source) -> Result<Vec<Token>, Vec<Diagnostic>> {
    let bytes = source.text().as_bytes();
    let mut tokens = Vec::new();
    let mut errors = Vec::new();
    let mut at = 0;

    while at < bytes.len() {
        let start = at;
        let kind = match bytes[at] {
            b' ' | b'\t' | b'\r' | b'\n' => {
                at += 1;
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at += 2;
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            b'"' => {
                at += 1;
                let mut closed = false;
                while at < bytes.len() {
                    match bytes[at] {
                        b'"' => {
                            at += 1;
                            closed = true;
                            break;
                        }
                        b'\\' => {
                            at += 1;
                            if !bytes.get(at).is_some_and(|byte| {
                                matches!(byte, b'"' | b'\\' | b'n' | b'r' | b't' | b'$')
                            }) {
                                push_error(
                                    &mut errors,
                                    Diagnostic::error(
                                        "unsupported string escape",
                                        make_span(
                                            source_id,
                                            start,
                                            at.saturating_add(1),
                                            bytes.len(),
                                        ),
                                    ),
                                );
                                while at < bytes.len() && !matches!(bytes[at], b'"' | b'\r' | b'\n')
                                {
                                    at += 1;
                                }
                                continue;
                            }
                            at += 1;
                        }
                        b'\r' | b'\n' => {
                            push_error(
                                &mut errors,
                                Diagnostic::error(
                                    "string literal cannot cross a line",
                                    make_span(source_id, start, at, bytes.len()),
                                ),
                            );
                            break;
                        }
                        _ => at += 1,
                    }
                }
                if !closed {
                    push_error(
                        &mut errors,
                        Diagnostic::error(
                            "unterminated string literal",
                            make_span(source_id, start, at, bytes.len()),
                        ),
                    );
                }
                TokenKind::String
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                at += 1;
                while bytes
                    .get(at)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    at += 1;
                }
                keyword(&source.text()[start..at]).map_or(TokenKind::Ident, TokenKind::Keyword)
            }
            byte if byte.is_ascii_digit() => {
                at += 1;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                TokenKind::Integer
            }
            b'{' => single(&mut at, TokenKind::LeftBrace),
            b'}' => single(&mut at, TokenKind::RightBrace),
            b'[' => single(&mut at, TokenKind::LeftBracket),
            b']' => single(&mut at, TokenKind::RightBracket),
            b'(' => single(&mut at, TokenKind::LeftParen),
            b')' => single(&mut at, TokenKind::RightParen),
            b'<' => single(&mut at, TokenKind::LeftAngle),
            b'>' => single(&mut at, TokenKind::RightAngle),
            b':' if bytes.get(at + 1) == Some(&b':') => double(&mut at, TokenKind::ColonColon),
            b':' => single(&mut at, TokenKind::Colon),
            b',' => single(&mut at, TokenKind::Comma),
            b'.' if bytes.get(at + 1) == Some(&b'.') => {
                at += 2;
                if bytes.get(at) == Some(&b'=') {
                    at += 1;
                    TokenKind::DotDotEqual
                } else {
                    TokenKind::DotDot
                }
            }
            b'.' => single(&mut at, TokenKind::Dot),
            b'=' if bytes.get(at + 1) == Some(&b'>') => double(&mut at, TokenKind::FatArrow),
            b'=' => single(&mut at, TokenKind::Equal),
            b'-' if bytes.get(at + 1) == Some(&b'>') => double(&mut at, TokenKind::Arrow),
            b'+' if bytes.get(at + 1) == Some(&b'+') => double(&mut at, TokenKind::PlusPlus),
            b';' => single(&mut at, TokenKind::Semicolon),
            _ => {
                at += source.text()[at..].chars().next().map_or(1, char::len_utf8);
                push_error(
                    &mut errors,
                    Diagnostic::error(
                        "unexpected character",
                        make_span(source_id, start, at, bytes.len()),
                    ),
                );
                if errors.len() == MAX_DIAGNOSTICS {
                    break;
                }
                continue;
            }
        };
        if tokens.len() == MAX_TOKENS {
            push_error(
                &mut errors,
                Diagnostic::error(
                    format!("source exceeds the {MAX_TOKENS}-token limit"),
                    make_span(source_id, start, at, bytes.len()),
                ),
            );
            break;
        }
        tokens.push(Token {
            kind,
            span: make_span(source_id, start, at, bytes.len()),
        });
        if errors.len() == MAX_DIAGNOSTICS {
            break;
        }
    }

    if errors.is_empty() {
        Ok(tokens)
    } else {
        Err(errors)
    }
}

fn single(at: &mut usize, kind: TokenKind) -> TokenKind {
    *at += 1;
    kind
}

fn double(at: &mut usize, kind: TokenKind) -> TokenKind {
    *at += 2;
    kind
}

fn push_error(errors: &mut Vec<Diagnostic>, error: Diagnostic) {
    if errors.len() < MAX_DIAGNOSTICS {
        errors.push(error);
    }
}

fn keyword(word: &str) -> Option<Keyword> {
    Some(match word {
        "enum" => Keyword::Enum,
        "erase" => Keyword::Erase,
        "fn" => Keyword::Fn,
        "in" => Keyword::In,
        "inputs" => Keyword::Inputs,
        "int" => Keyword::Int,
        "let" => Keyword::Let,
        "match" => Keyword::Match,
        "mod" => Keyword::Mod,
        "opaque" => Keyword::Opaque,
        "outputs" => Keyword::Outputs,
        "owner" => Keyword::Owner,
        "resource" => Keyword::Resource,
        "self" => Keyword::SelfValue,
        "str" => Keyword::Str,
        "struct" => Keyword::Struct,
        "type" => Keyword::Type,
        "use" => Keyword::Use,
        "value" => Keyword::Value,
        "where" => Keyword::Where,
        _ => return None,
    })
}

pub(crate) fn is_keyword(word: &str) -> bool {
    keyword(word).is_some()
}

fn make_span(source_id: SourceId, start: usize, end: usize, limit: usize) -> Span {
    Span::new(
        source_id,
        u32::try_from(start.min(limit)).unwrap_or(u32::MAX),
        u32::try_from(end.min(limit)).unwrap_or(u32::MAX),
    )
}
