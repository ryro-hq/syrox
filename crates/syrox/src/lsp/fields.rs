use super::Server;
use serde_json::{Value, json};
use syrox_lang::{
    LineIndex, ParsedFile, ResolvedTarget, SemanticAnalysis, SourceId, SyntaxToken,
    SyntaxTokenKind as Token, Ty,
};

impl Server {
    pub(super) fn complete_fields(
        &self,
        parsed: &ParsedFile,
        offset: u32,
        analysis: &SemanticAnalysis,
        id: SourceId,
    ) -> Option<Value> {
        let text = parsed.source().text();
        if !text.is_char_boundary(offset as usize) {
            return None;
        }
        let tokens: Vec<_> = parsed
            .tokens()
            .filter(|token| !token.kind.is_trivia())
            .collect();
        if parsed.tokens().any(|token| {
            token.span.start() <= offset
                && offset < token.span.end()
                && matches!(token.kind, Token::String | Token::Comment)
        }) {
            return None;
        }
        let mut index = tokens.partition_point(|token| token.span.end() <= offset);
        let (start, end, prefix) = if let Some(token) = tokens
            .get(index)
            .filter(|token| token.kind == Token::Ident && token.span.start() <= offset)
        {
            (
                token.span.start(),
                token.span.end(),
                &text[token.span.start() as usize..offset as usize],
            )
        } else if index > 0
            && tokens[index - 1].kind == Token::Ident
            && tokens[index - 1].span.end() == offset
        {
            index -= 1;
            (
                tokens[index].span.start(),
                offset,
                &text[tokens[index].span.start() as usize..offset as usize],
            )
        } else {
            (offset, offset, "")
        };
        if index == 0 || tokens[index - 1].kind != Token::Dot {
            return None;
        }
        let dot = index - 1;
        if let Some(previous) = dot.checked_sub(1).and_then(|index| tokens.get(index))
            && matches!(previous.kind, Token::RightParen | Token::RightBracket)
        {
            let ty = analysis
                .expression_at(id, previous.span.start())
                .map(syrox_lang::CheckedExpression::ty)
                .filter(|ty| ty.is_known())
                .cloned()
                .or_else(|| receiver_result(text, &tokens[..dot], analysis, id, offset));
            if let Some(ty) = ty {
                return self
                    .field_candidates(parsed, offset, analysis, id, &ty, start, end, prefix);
            }
        }
        let mut beginning = dot;
        while beginning > 0
            && matches!(
                tokens[beginning - 1].kind,
                Token::Ident | Token::Dot | Token::ColonColon
            )
        {
            beginning -= 1;
        }
        let receiver = tokens[beginning..dot]
            .iter()
            .map(|token| &text[token.span.start() as usize..token.span.end() as usize])
            .collect::<String>();
        let mut segments = receiver.split('.');
        let parts = segments
            .next()?
            .split("::")
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let target = analysis.lookup_path(id, offset, &parts)?;
        let mut ty = match target {
            ResolvedTarget::Local(local) => analysis.local_type(local)?.clone(),
            ResolvedTarget::Item(item) => analysis.output_type(item)?.clone(),
            _ => return None,
        };
        for field in segments {
            ty = analysis
                .fields_at(id, offset, &ty)
                .into_iter()
                .find(|candidate| candidate.name == field)?
                .ty;
        }
        self.field_candidates(parsed, offset, analysis, id, &ty, start, end, prefix)
    }

    #[allow(clippy::too_many_arguments)]
    fn field_candidates(
        &self,
        parsed: &ParsedFile,
        offset: u32,
        analysis: &SemanticAnalysis,
        id: SourceId,
        ty: &Ty,
        start: u32,
        end: u32,
        prefix: &str,
    ) -> Option<Value> {
        let lines = LineIndex::new(parsed.source());
        let start = lines.position(start, self.encoding)?;
        let end = lines.position(end, self.encoding)?;
        let items = analysis.fields_at(id,offset,ty).into_iter().filter(|field|field.name.starts_with(prefix)).map(|field|json!({"label":field.name,"kind":5,"detail":analysis.display_type(&field.ty),"textEdit":{"range":{"start":{"line":start.line,"character":start.character},"end":{"line":end.line,"character":end.character}},"newText":field.name}})).collect::<Vec<_>>();
        Some(json!({"isIncomplete":false,"items":items}))
    }
}

fn receiver_result(
    text: &str,
    tokens: &[&SyntaxToken],
    analysis: &SemanticAnalysis,
    source: SourceId,
    offset: u32,
) -> Option<Ty> {
    if tokens.last()?.kind != Token::RightParen {
        return None;
    }
    let mut depth = 0usize;
    let open = (0..tokens.len()).rev().find(|index| {
        match tokens[*index].kind {
            Token::RightParen => depth += 1,
            Token::LeftParen => depth -= 1,
            _ => {}
        }
        depth == 0
    })?;
    let mut start = open;
    while start > 0 && matches!(tokens[start - 1].kind, Token::Ident | Token::ColonColon) {
        start -= 1;
    }
    let (path, call) = if start < open {
        (&tokens[start..open], true)
    } else {
        (&tokens[open + 1..tokens.len() - 1], false)
    };
    if path.is_empty()
        || path.len().is_multiple_of(2)
        || path.iter().enumerate().any(|(index, token)| {
            token.kind
                != if index.is_multiple_of(2) {
                    Token::Ident
                } else {
                    Token::ColonColon
                }
        })
    {
        return None;
    }
    let parts = path
        .iter()
        .filter(|token| token.kind == Token::Ident)
        .map(|token| text[token.span.start() as usize..token.span.end() as usize].to_owned())
        .collect::<Vec<_>>();
    let target = analysis.lookup_path(source, offset, &parts)?;
    let ty = match target {
        ResolvedTarget::Local(local) => analysis.local_type(local)?,
        ResolvedTarget::Item(item) => analysis
            .function_type(item)
            .or_else(|| analysis.output_type(item))?,
        _ => return None,
    };
    if call {
        if let Ty::Function { result, .. } = ty {
            Some((**result).clone())
        } else {
            None
        }
    } else {
        Some(ty.clone())
    }
}
