use serde_json::{Value, json};
use syrox_lang::{
    LineIndex, ParsedFile, SemanticAnalysis, SourceId, SyntaxKind, SyntaxToken,
    SyntaxTokenKind as Token,
};

use super::Server;

impl Server {
    pub(super) fn complete_construction(
        &self,
        parsed: &ParsedFile,
        offset: u32,
        analysis: &SemanticAnalysis,
        source: SourceId,
    ) -> Option<Value> {
        let text = parsed.source().text();
        if !text.is_char_boundary(offset as usize) {
            return None;
        }
        let node = parsed
            .syntax()
            .nodes()
            .filter(|node| {
                node.kind() == SyntaxKind::StructLiteral
                    && node.span().start() < offset
                    && (offset < node.span().end()
                        || offset == node.span().end() && offset as usize == text.len())
            })
            .min_by_key(|node| node.span().end() - node.span().start())?;
        if parsed.tokens().any(|token| {
            matches!(token.kind, Token::String | Token::Comment)
                && token.span.start() <= offset
                && offset < token.span.end()
        }) {
            return None;
        }
        let tokens: Vec<_> = parsed
            .tokens()
            .filter(|token| {
                !token.kind.is_trivia()
                    && token.span.start() >= node.span().start()
                    && token.span.end() <= node.span().end()
            })
            .collect();
        let open = tokens.first()?;
        if open.kind != Token::LeftBrace {
            return None;
        }
        let segments = field_segments(&tokens, node.span().end().max(offset));
        let (index, (_, _, current)) = segments
            .iter()
            .enumerate()
            .find(|(_, (start, end, _))| *start <= offset && offset <= *end)?;
        let name = current.first().filter(|token| token.kind == Token::Ident);
        let editing_name = name.is_some_and(|name| name.span.start() <= offset);
        let (start, end, prefix) = if let Some(name) = name {
            if offset > name.span.end() {
                return None;
            }
            if offset < name.span.start() {
                (offset, offset, "")
            } else {
                (
                    name.span.start(),
                    name.span.end(),
                    &text[name.span.start() as usize..offset as usize],
                )
            }
        } else if current.is_empty() {
            (offset, offset, "")
        } else {
            return None;
        };
        let existing: Vec<_> = segments
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index || !editing_name)
            .filter_map(|(_, (_, _, segment))| {
                let first = segment.first()?;
                (first.kind == Token::Ident && segment.get(1)?.kind == Token::Equal)
                    .then(|| &text[first.span.start() as usize..first.span.end() as usize])
            })
            .collect();
        let ty = analysis.expression_at(source, open.span.start())?.ty();
        let lines = LineIndex::new(parsed.source());
        let start = lines.position(start, self.encoding)?;
        let end = lines.position(end, self.encoding)?;
        let range = json!({"start":{"line":start.line,"character":start.character},"end":{"line":end.line,"character":end.character}});
        let fields = analysis.construction_fields_at(source, offset, ty);
        let incomplete = fields.len() >= 256 || analysis.editor_facts_truncated();
        let items: Vec<_> = fields.into_iter().filter(|field| field.name.starts_with(prefix) && !existing.contains(&field.name.as_str()))
            .map(|field| {
                let status = if field.has_default { "default" } else { "required" };
                json!({"label":field.name,"kind":5,"detail":format!("{} ({status})", analysis.display_type(&field.ty)),
                    "sortText":format!("{}:{}",u8::from(field.has_default),field.name),
                    "textEdit":{"range":range,"newText":field.name}})
            }).collect();
        Some(json!({"isIncomplete":incomplete,"items":items}))
    }
}

/// Only top-level semicolons divide fields. Nested literals and closure bodies
/// retain their own fields and statement separators.
fn field_segments<'a>(
    tokens: &[&'a SyntaxToken],
    end: u32,
) -> Vec<(u32, u32, Vec<&'a SyntaxToken>)> {
    let mut segments = Vec::new();
    let mut start = tokens[0].span.end();
    let mut current = Vec::new();
    let mut depth = 0usize;
    for token in tokens.iter().skip(1) {
        if depth == 0 && matches!(token.kind, Token::Semicolon | Token::RightBrace) {
            segments.push((start, token.span.start(), std::mem::take(&mut current)));
            start = token.span.end();
            if token.kind == Token::RightBrace {
                return segments;
            }
            continue;
        }
        match token.kind {
            Token::LeftBrace | Token::LeftParen | Token::LeftBracket => depth += 1,
            Token::RightBrace | Token::RightParen | Token::RightBracket => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        current.push(*token);
    }
    segments.push((start, end, current));
    segments
}
