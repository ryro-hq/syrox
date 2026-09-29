use std::{fmt, mem::size_of, sync::Arc};

use super::{CanonicalType, Eval, Evaluator, Value};
use crate::{CanonicalItemIdentity, SourceDomainId, Span};

#[cfg(test)]
mod tests;

/// An opaque identity within one evaluation operation. Copies retain identity;
/// distinct constructions and distinct operations never compare equal. This
/// token is not a persistent recipe identity or an Action digest.
#[derive(Clone)]
pub struct MemoId {
    operation: Arc<()>,
    index: usize,
}

impl PartialEq for MemoId {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && Arc::ptr_eq(&self.operation, &other.operation)
    }
}
impl Eq for MemoId {}
impl fmt::Debug for MemoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MemoId").field(&self.index).finish()
    }
}

#[derive(Default)]
pub(in crate::evaluator) struct MemoArena {
    operation: Arc<()>,
    entries: Vec<Entry>,
    active: Vec<usize>,
}

struct Entry {
    owner: SourceDomainId,
    origin: Span,
    label: Arc<CanonicalItemIdentity>,
    state: State,
}

enum State {
    Pending(Value),
    Evaluating,
    Ready(Value),
    Failed,
}

impl Evaluator<'_> {
    pub(super) fn memoize(&mut self, callable: Value, span: Span) -> Eval<Value> {
        if matches!(callable, Value::MemoizedFunction { .. }) {
            return Ok(callable);
        }
        let ty = callable.canonical_type();
        if !matches!(ty.as_ref(), CanonicalType::Function { parameters, once: false, .. } if parameters.is_empty())
            || callable.affine()
        {
            return self.fail(
                span,
                "memoize requires a reusable function with no arguments",
            );
        }
        self.expand(2 * size_of::<Entry>() + size_of::<MemoId>(), span)?;
        let (origin, label) = match &callable {
            Value::Function { item, .. } => (
                span,
                self.index
                    .identity(*item)
                    .unwrap_or_else(|| self.root.clone()),
            ),
            Value::Closure { body, .. } => (body.span, self.root.clone()),
            _ => (span, self.root.clone()),
        };
        let id = MemoId {
            operation: self.memos.operation.clone(),
            index: self.memos.entries.len(),
        };
        self.memos.entries.push(Entry {
            owner: self.project_context,
            origin,
            label,
            state: State::Pending(callable),
        });
        Ok(Value::MemoizedFunction { ty, id })
    }

    pub(in crate::evaluator) fn force_memo(&mut self, id: &MemoId, span: Span) -> Eval<Value> {
        self.tick(span)?;
        if !Arc::ptr_eq(&id.operation, &self.memos.operation)
            || id.index >= self.memos.entries.len()
        {
            return self.fail(
                span,
                "memoized reference belongs to another evaluation operation",
            );
        }
        let state = std::mem::replace(&mut self.memos.entries[id.index].state, State::Evaluating);
        match state {
            State::Ready(value) => {
                let result = Self::copy_value_ref(self, &value, span, 0);
                self.memos.entries[id.index].state = State::Ready(value);
                result
            }
            State::Failed => {
                self.memos.entries[id.index].state = State::Failed;
                self.fail(span, "memoized computation previously failed")
            }
            State::Evaluating => {
                let mut path = String::new();
                for offset in 0..=self.memos.active.len() {
                    use std::fmt::Write as _;
                    let index = self.memos.active.get(offset).copied().unwrap_or(id.index);
                    let bytes = self.memos.entries[index]
                        .label
                        .path()
                        .iter()
                        .map(String::len)
                        .sum::<usize>()
                        .saturating_add(
                            self.memos.entries[index]
                                .label
                                .path()
                                .len()
                                .saturating_mul(2),
                        )
                        .saturating_add(64);
                    self.tick(span)?;
                    self.expand(bytes.saturating_mul(2), span)?;
                    let entry = &self.memos.entries[index];
                    if !path.is_empty() {
                        path.push_str(" -> ");
                    }
                    write!(
                        path,
                        "{}:{}@{}:{}",
                        entry.label.domain().as_u32(),
                        entry.label.path().join("::"),
                        entry.origin.source_id().index(),
                        entry.origin.start()
                    )
                    .expect("String write");
                }
                self.fail(span, format!("cycle in memoized computation: {path}"))
            }
            State::Pending(callable) => {
                let result = self.compute_memo(id.index, callable, span);
                if result.is_err() {
                    self.memos.entries[id.index].state = State::Failed;
                }
                result
            }
        }
    }

    fn compute_memo(&mut self, index: usize, callable: Value, span: Span) -> Eval<Value> {
        // Effects include reads of claimful cached outputs, preventing a memo
        // from laundering a root-scoped claim into a shareable plain payload.
        let claim_effects = self.claim_effects;
        self.expand(size_of::<usize>(), span)?;
        self.memos.active.push(index);
        let previous =
            std::mem::replace(&mut self.project_context, self.memos.entries[index].owner);
        let result = self.invoke_value(callable, Vec::new(), span);
        self.project_context = previous;
        self.memos.active.pop();
        let value = result?;
        if value.affine() || self.claim_effects != claim_effects {
            return self.fail(
                span,
                "memoized computation must return a reusable value without resource claims",
            );
        }
        let copy = Self::copy_value_ref(self, &value, span, 0)?;
        self.memos.entries[index].state = State::Ready(value);
        Ok(copy)
    }
}
