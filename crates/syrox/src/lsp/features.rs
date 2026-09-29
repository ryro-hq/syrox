use std::io::{self, Write};

use serde_json::{Value, json};
use syrox_lang::{
    AnalysisCancellation, Diagnostic, LineIndex, PositionEncoding, SemanticAnalysis, Severity,
    Span, Ty,
};

use super::{Server, document_uri, position, transport, worker};

impl Server {
    pub(super) fn publish(
        &self,
        result: Option<&worker::ResultSet>,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        let snapshot = self.host.snapshot();
        let cancellation = AnalysisCancellation::default();
        for uri in snapshot.document_names() {
            let document = snapshot.document(uri).expect("registered document");
            let lines = document
                .line_index(&cancellation)
                .expect("active syntax query");
            let mut diagnostics = Vec::new();
            let semantic = result.and_then(|result| {
                result
                    .analysis
                    .as_ref()
                    .ok()
                    .map(|analysis| (result, analysis))
            });
            if let Some((result, analysis)) =
                semantic.filter(|(result, _)| result.paths.values().any(|path| path == uri))
            {
                for diagnostic in analysis.diagnostics().iter().filter(|diagnostic| {
                    result
                        .paths
                        .get(&diagnostic.span.source_id())
                        .is_some_and(|path| path == uri)
                }) {
                    if analysis
                        .sources()
                        .get(diagnostic.span.source_id())
                        .is_some()
                    {
                        let mut value = diagnostic_json(diagnostic, lines, self.encoding);
                        let related: Vec<_> = diagnostic.related.iter().filter_map(|related| {
                            let uri = result.paths.get(&related.span.source_id())?;
                            let source = analysis.sources().get(related.span.source_id())?;
                            Some(json!({"location":{"uri":uri,"range":range(&LineIndex::new(source), related.span, self.encoding)?},"message":related.message}))
                        }).collect();
                        if !related.is_empty() {
                            value["relatedInformation"] = json!(related);
                        }
                        diagnostics.push(value);
                    }
                }
            } else if let Ok(local) = document.diagnostics(&cancellation) {
                diagnostics.extend(
                    local
                        .iter()
                        .map(|diagnostic| diagnostic_json(diagnostic, lines, self.encoding)),
                );
            }
            diagnostics.sort_by_key(Value::to_string);
            diagnostics.dedup();
            transport::write_message(
                writer,
                &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"version":document.overlay_version(),"diagnostics":diagnostics}}),
            )?;
        }
        Ok(())
    }

    pub(super) fn feature(&self, method: &str, params: &Value) -> Result<Value, String> {
        if method == "syrox/readSource" {
            return self.read_source(params);
        }
        let uri = document_uri(params)?;
        let document = if let Some(source) = self
            .latest
            .as_ref()
            .and_then(|result| result.virtual_source(&uri))
        {
            let mut host = syrox_lang::AnalysisHost::default();
            host.set_disk(&uri, source.text())
                .map_err(|error| error.to_string())?;
            host.snapshot().document(&uri).expect("virtual document")
        } else {
            self.host
                .snapshot()
                .document(&uri)
                .ok_or("document is not open or virtual source expired")?
        };
        let cancellation = AnalysisCancellation::default();
        let parsed = document
            .parsed(&cancellation)
            .map_err(|error| error.to_string())?;
        let lines = document
            .line_index(&cancellation)
            .map_err(|error| error.to_string())?;
        let semantic = self.latest.as_ref().and_then(|result| {
            let analysis = result.analysis.as_ref().ok()?;
            let id = analysis
                .sources()
                .iter()
                .find(|(id, _)| result.uri(*id) == uri)?
                .0;
            Some((result, analysis, id))
        });
        if method == "textDocument/inlayHint" {
            return semantic.map_or_else(
                || Ok(json!([])),
                |(result, analysis, id)| {
                    self.inlay_hints(params, result, analysis, id, lines, false)
                },
            );
        }
        if method == "textDocument/semanticTokens/full" {
            return semantic.map_or_else(
                || Ok(json!({"data":[]})),
                |(result, analysis, id)| {
                    self.semantic_tokens(analysis, id, lines, result.generation)
                },
            );
        }
        if method == "textDocument/documentSymbol" {
            if let Some((_, analysis, id)) = semantic {
                return Ok(json!(analysis.symbols(&cancellation).map_err(|error| error.to_string())?
                    .filter(|symbol| symbol.declaration.source_id() == id)
                    .filter(|symbol| !matches!(symbol.kind, syrox_lang::SemanticSymbolKind::Variable | syrox_lang::SemanticSymbolKind::Parameter | syrox_lang::SemanticSymbolKind::TypeParameter))
                    .map(|symbol| json!({"name":symbol.name,"kind":super::presentation::symbol_kind(symbol.kind),"range":range(lines,symbol.declaration,self.encoding),"selectionRange":range(lines,symbol.declaration,self.encoding)})).collect::<Vec<_>>()));
            }
            return Ok(Value::Array(parsed.function_signatures().filter_map(|signature| {
                Some(json!({"name":signature.name.text,"kind":12,"detail":path_label(&signature.module),"range":range(lines, signature.span, self.encoding)?,"selectionRange":range(lines, signature.name.span, self.encoding)?}))
            }).collect()));
        }
        let offset = lines
            .offset(position(&params["position"])?, self.encoding)
            .ok_or("invalid cursor position")?;
        if matches!(
            method,
            "textDocument/references"
                | "textDocument/documentHighlight"
                | "textDocument/definition"
                | "textDocument/typeDefinition"
        ) {
            return semantic.map_or(Ok(Value::Null), |(result, analysis, id)| {
                self.navigate(method, params, result, analysis, id, offset)
            });
        }
        if method == "textDocument/completion" {
            return Ok(self.complete(
                parsed,
                offset,
                semantic.map(|(_, analysis, id)| (analysis.as_ref(), id)),
            ));
        }
        if method == "textDocument/signatureHelp" {
            return Ok(Self::signature_help(
                parsed,
                offset,
                semantic.map(|(_, analysis, id)| (analysis.as_ref(), id)),
            ));
        }
        if let Some((_, analysis, id)) = semantic
            && let Some(hover) = Self::semantic_hover(analysis, id, offset)
        {
            return Ok(hover);
        }
        Ok(parsed.function_signatures().find(|signature| signature.name.span.start() <= offset && offset <= signature.name.span.end()).map_or(Value::Null, |signature| {
            let text = &document.source().text()[signature.span.start() as usize..signature.span.end() as usize];
            json!({"contents":{"kind":"plaintext","value":text},"range":range(lines, signature.name.span, self.encoding)})
        }))
    }

    fn read_source(&self, params: &Value) -> Result<Value, String> {
        let uri = params["uri"].as_str().ok_or("missing virtual source URI")?;
        let source = self
            .latest
            .as_ref()
            .and_then(|result| result.virtual_source(uri))
            .ok_or("unknown or expired virtual source URI")?;
        Ok(json!({"uri":uri,"languageId":"syrox","text":source.text(),"name":source.name()}))
    }
}

fn diagnostic_json(
    diagnostic: &Diagnostic,
    lines: &LineIndex,
    encoding: PositionEncoding,
) -> Value {
    let message = diagnostic.note.as_ref().map_or_else(
        || diagnostic.message.clone(),
        |note| format!("{}\n{note}", diagnostic.message),
    );
    json!({"range":range(lines, diagnostic.span, encoding).unwrap_or_else(|| json!({"start":{"line":0,"character":0},"end":{"line":0,"character":0}})),"severity":match diagnostic.severity {Severity::Error=>1,Severity::Warning=>2},"code":diagnostic.code.as_str(),"source":"syrox","message":message})
}

pub(super) fn range(lines: &LineIndex, span: Span, encoding: PositionEncoding) -> Option<Value> {
    // LSP positions cannot represent the interior of CRLF. Enclose that
    // newline if a lexical diagnostic ends after CR but before LF.
    let start = lines.position(span.start(), encoding).or_else(|| {
        span.start()
            .checked_sub(1)
            .and_then(|offset| lines.position(offset, encoding))
    })?;
    let end = lines.position(span.end(), encoding).or_else(|| {
        span.end()
            .checked_add(1)
            .and_then(|offset| lines.position(offset, encoding))
    })?;
    Some(
        json!({"start":{"line":start.line,"character":start.character},"end":{"line":end.line,"character":end.character}}),
    )
}

pub(super) fn path_label(parts: &[String]) -> String {
    let mut label = String::new();
    let mut remaining = 256usize;
    for (index, part) in parts.iter().enumerate() {
        if remaining < 3 {
            label.push('…');
            break;
        }
        if index > 0 {
            label.push_str("::");
            remaining -= 2;
        }
        for character in part.chars() {
            if remaining == 0 {
                label.push('…');
                return label;
            }
            label.push(character);
            remaining -= 1;
        }
    }
    label
}

pub(super) fn display_type(
    analysis: &SemanticAnalysis,
    ty: &Ty,
    remaining: &mut usize,
    depth: usize,
) -> String {
    if *remaining == 0 || depth == 32 {
        return "…".into();
    }
    *remaining -= 1;
    analysis.display_type(ty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_ending_inside_crlf_is_mapped_to_an_enclosing_lsp_range() {
        let source = syrox_lang::Source::new(
            "crlf.srx",
            "value S(str);\r\noutputs { item: S = \"bad\\\r\n\"; }",
        )
        .unwrap();
        let parsed = syrox_lang::parse_file(&source);
        let diagnostic = parsed
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.message == "unsupported string escape")
            .unwrap();
        let value = diagnostic_json(
            diagnostic,
            &LineIndex::new(&source),
            PositionEncoding::Utf16,
        );
        assert_eq!(value["range"]["start"]["line"], 1);
        assert_eq!(value["range"]["end"], json!({"line":2,"character":0}));
    }
}
