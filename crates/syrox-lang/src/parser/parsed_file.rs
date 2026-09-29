use super::{Diagnostic, Item, ParsedProgram, Parser, Source, SourceId, SyntaxTree, lex};

/// Complete syntactic header, retained even if the body is malformed. Types are
/// unresolved; visibility and module membership still require semantic checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionSignature {
    /// Lexical namespace relative to this file's project-assigned module.
    pub module: std::sync::Arc<[String]>,
    pub public: bool,
    pub name: crate::Ident,
    pub type_parameters: Vec<crate::TypeParameter>,
    pub parameters: Vec<crate::Parameter>,
    pub result: Option<crate::Type>,
    pub span: crate::Span,
}

pub use crate::lexer::{Keyword, Token as SyntaxToken, TokenKind as SyntaxTokenKind};

#[cfg(test)]
mod tests;

/// One file's lossless token stream and recovered declarations.
/// Recovered items are editor data; only `into_program` admits a valid program.
/// Incomplete grammar contexts and missing syntax are retained in `syntax`.
#[derive(Clone, Debug)]
pub struct ParsedFile {
    source: Source,
    syntax: SyntaxTree,
    program: ParsedProgram,
    diagnostics: Vec<Diagnostic>,
    signatures: Vec<FunctionSignature>,
    pub(crate) incomplete_bodies: Vec<crate::Span>,
}

impl ParsedFile {
    pub(crate) fn recovered_program(&self) -> &ParsedProgram {
        &self.program
    }
    pub const fn source(&self) -> &Source {
        &self.source
    }

    pub fn tokens(&self) -> impl ExactSizeIterator<Item = &SyntaxToken> {
        self.syntax.tokens()
    }

    pub const fn syntax(&self) -> &SyntaxTree {
        &self.syntax
    }

    pub fn recovered_items(&self) -> impl ExactSizeIterator<Item = &Item> {
        self.program.items.iter()
    }

    /// Name spans of functions whose recovered AST has a placeholder empty
    /// body. Such bodies are unavailable, not successfully parsed empty blocks.
    pub fn incomplete_function_bodies(&self) -> impl ExactSizeIterator<Item = crate::Span> + '_ {
        self.incomplete_bodies.iter().copied()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn function_signatures(&self) -> impl ExactSizeIterator<Item = &FunctionSignature> {
        self.signatures.iter()
    }

    pub fn into_program(self) -> Result<ParsedProgram, Vec<Diagnostic>> {
        if self.diagnostics.is_empty() {
            Ok(self.program)
        } else {
            Err(self.diagnostics)
        }
    }
}

/// Parse an editor buffer without losing valid declarations or original tokens
/// when another declaration is malformed. Uses the same grammar as `analyze`.
pub fn parse_file(source: &Source) -> ParsedFile {
    parse_file_in(SourceId::SINGLE, source)
}

pub(crate) fn parse_file_in(source_id: SourceId, source: &Source) -> ParsedFile {
    parse_file_cancellable(source_id, source, None).expect("uncancelled parse")
}

pub(crate) fn parse_file_cancellable(
    source_id: SourceId,
    source: &Source,
    cancellation: Option<&crate::AnalysisCancellation>,
) -> Result<ParsedFile, crate::AnalysisCancelled> {
    if let Some(cancel) = cancellation {
        cancel.check()?;
    }
    let lexed = match cancellation {
        Some(cancel) => crate::lexer::lex_cancellable(source_id, source, Some(cancel))?,
        None => lex(source_id, source),
    };
    let significant: Vec<_> = lexed
        .tokens
        .iter()
        .copied()
        .filter(|token| !token.kind.is_trivia())
        .collect();
    if let Some(cancel) = cancellation {
        cancel.check()?;
    }
    let mut parser = Parser::new(source_id, source, &significant);
    parser.cancellation = cancellation;
    let parsed = parser.parse();
    if let Some(cancel) = cancellation {
        cancel.check()?;
    }
    let mut diagnostics = lexed.diagnostics;
    diagnostics.extend(
        parsed
            .diagnostics
            .into_iter()
            .take(crate::MAX_DIAGNOSTICS.saturating_sub(diagnostics.len())),
    );
    Ok(ParsedFile {
        source: source.clone(),
        syntax: SyntaxTree::build(
            source_id,
            u32::try_from(source.text().len()).expect("source size is bounded"),
            lexed.tokens,
            parsed.events,
            cancellation,
        )?,
        program: parsed.program,
        diagnostics,
        signatures: parsed.signatures,
        incomplete_bodies: parsed.incomplete_bodies,
    })
}
