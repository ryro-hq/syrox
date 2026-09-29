use std::io::{self, Write};

use serde_json::Value;

use super::{Server, document_uri, position, response, response_error};

#[derive(Debug)]
pub(super) struct PendingCompletion {
    id: Value,
    params: Value,
    method: String,
}

impl Server {
    pub(super) fn completion_request(
        &mut self,
        id: Value,
        params: &Value,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        self.semantic_request("textDocument/completion", id, params, writer)
    }

    pub(super) fn semantic_request(
        &mut self,
        method: &str,
        id: Value,
        params: &Value,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        let document = document_uri(params)
            .ok()
            .and_then(|uri| self.host.snapshot().document(&uri));
        if self.latest.is_none()
            && self.generation > 0
            && document.is_some()
            && (position(&params["position"]).is_ok()
                || matches!(
                    method,
                    "textDocument/inlayHint"
                        | "textDocument/semanticTokens/full"
                        | "textDocument/documentSymbol"
                ))
        {
            if self.pending_completions.len() >= 64 {
                return response_error(
                    writer,
                    id,
                    -32803,
                    "semantic request queue full; request again",
                );
            }
            self.pending_completions.push(PendingCompletion {
                id,
                params: params.clone(),
                method: method.into(),
            });
            return Ok(());
        }
        self.semantic_response(method, id, params, writer)
    }

    fn semantic_response(
        &self,
        method: &str,
        id: Value,
        params: &Value,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        match self.feature(method, params) {
            Ok(result) => response(writer, id, result),
            Err(error) => response_error(writer, id, -32602, &error),
        }
    }

    pub(super) fn finish_completions(&mut self, writer: &mut impl Write) -> io::Result<()> {
        for pending in std::mem::take(&mut self.pending_completions) {
            self.semantic_response(&pending.method, pending.id, &pending.params, writer)?;
        }
        Ok(())
    }

    pub(super) fn invalidate_completions(&mut self, writer: &mut impl Write) -> io::Result<()> {
        for pending in std::mem::take(&mut self.pending_completions) {
            // Normal didChange is not ContentModified (LSP errorCodes). Finish
            // the superseded query without applying its cursor to newer text.
            // The client owns cancellation/retriggering for its new document.
            let empty = match pending.method.as_str() {
                "textDocument/completion" => serde_json::json!({"isIncomplete":true,"items":[]}),
                "textDocument/semanticTokens/full" => serde_json::json!({"data":[]}),
                "textDocument/inlayHint"
                | "textDocument/documentSymbol"
                | "textDocument/references"
                | "textDocument/documentHighlight" => serde_json::json!([]),
                _ => Value::Null,
            };
            response(writer, pending.id, empty)?;
        }
        Ok(())
    }

    pub(super) fn cancel_completion(
        &mut self,
        id: &Value,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        if let Some(index) = self
            .pending_completions
            .iter()
            .position(|pending| pending.id == *id)
        {
            let pending = self.pending_completions.remove(index);
            response_error(writer, pending.id, -32800, "completion request cancelled")?;
        }
        Ok(())
    }
}
