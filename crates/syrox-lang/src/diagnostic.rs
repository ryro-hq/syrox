use ariadne::{Config, Label, Report, ReportKind};
use thiserror::Error;

use crate::{Source, SourceSet, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    pub note: Option<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            span,
            note: None,
        }
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn render(&self, source: &Source) -> String {
        self.render_source(source)
    }

    pub fn render_from(&self, sources: &SourceSet) -> Option<String> {
        sources
            .get(self.span.source_id())
            .map(|source| self.render_source(source))
    }

    fn render_source(&self, source: &Source) -> String {
        let kind = match self.severity {
            Severity::Error => ReportKind::Error,
            Severity::Warning => ReportKind::Warning,
        };
        let text = source.text();
        let byte_range = self.span.range();
        let location = (
            source.name(),
            byte_to_char_offset(text, byte_range.start)..byte_to_char_offset(text, byte_range.end),
        );
        let mut report = Report::build(kind, location.clone())
            .with_config(Config::default().with_color(false))
            .with_message(&self.message)
            .with_label(Label::new(location).with_message(&self.message));
        if let Some(note) = &self.note {
            report = report.with_note(note);
        }

        let mut output = Vec::new();
        report
            .finish()
            .write(
                (source.name(), ariadne::Source::from(source.text())),
                &mut output,
            )
            .expect("writing diagnostics to a byte buffer cannot fail");
        String::from_utf8(output).expect("ariadne diagnostics are valid UTF-8")
    }
}

fn byte_to_char_offset(text: &str, byte_offset: usize) -> usize {
    text.char_indices()
        .take_while(|(offset, _)| *offset < byte_offset)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_translates_byte_spans_after_multibyte_text() {
        let source = Source::new("unicode.srx", "é @").unwrap();
        let errors = crate::analyze(&source).unwrap_err();
        let rendered = errors[1].render(&source);

        assert!(rendered.contains("unicode.srx:1:3"));
        assert!(rendered.contains("é @"));
    }

    #[test]
    fn source_set_rendering_selects_the_span_source() {
        let mut sources = SourceSet::new();
        sources.add("valid.srx", "type Good = T;").unwrap();
        let invalid = sources.add("invalid.srx", "type Bad T;").unwrap();
        let errors = crate::parse_sources(&sources).unwrap_err();
        let error = errors
            .iter()
            .find(|error| error.span.source_id() == invalid)
            .unwrap();
        let rendered = error.render_from(&sources).unwrap();

        assert!(rendered.contains("invalid.srx:1:"));
        assert!(rendered.contains("type Bad T;"));
        assert!(!rendered.contains("type Good = T;"));
    }
}
