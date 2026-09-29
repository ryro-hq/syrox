//! Pure, bounded analysis of Syrox source text.

mod analysis;
mod ast;
mod checker;
mod diagnostic;
mod evaluator;
mod lexer;
mod line_index;
mod parser;
mod resolver;
mod source;

pub use analysis::{
    AnalysisCancellation, AnalysisCancelled, AnalysisHost, AnalysisLimits, AnalysisRevision,
    AnalysisSnapshot, AnalysisUpdateError, DocumentSnapshot, InterfaceNamespaceDependency,
    OwnerTypeDependencies, SemanticAnalysis, SemanticAnalysisError, SemanticOccurrence,
    SemanticSymbol, SemanticSymbolKind, SyntaxFix, TypeInterface,
};
pub use ast::*;
pub use checker::*;
pub use diagnostic::{Diagnostic, DiagnosticCode, RelatedDiagnostic, Severity, SyntaxExpectation};
pub use evaluator::*;
pub use line_index::{LineIndex, PositionEncoding, TextPosition};
pub use parser::{
    FunctionSignature, ParsedFile, SyntaxElement, SyntaxKeyword, SyntaxKind, SyntaxNode,
    SyntaxToken, SyntaxTokenKind, SyntaxTree, analyze, parse_file, parse_sources,
};
pub use resolver::*;
pub use source::{Source, SourceDomainId, SourceError, SourceId, SourceSet, Span};

/// Maximum accepted bytes in one source file.
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;
/// Maximum number of source files in one source set.
pub const MAX_SOURCES: usize = 65_536;
/// Maximum lexical tokens, including trivia, scanned in one source file.
pub const MAX_TOKENS: usize = 262_144;
/// Maximum nested delimiter and module depth.
pub const MAX_DEPTH: usize = 128;
/// Maximum diagnostics emitted by one analysis phase.
pub const MAX_DIAGNOSTICS: usize = 256;
