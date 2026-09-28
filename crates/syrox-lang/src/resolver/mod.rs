//! Name resolution for parsed Syrox programs.
//!
//! Canonical identity is structural: module and item paths are vectors of
//! segments, never formatted strings. The resolver collects the complete
//! namespace before looking up references, so declaration order is irrelevant.

mod body;
mod lookup;
mod model;
mod namespace;

pub use model::*;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Block, Diagnostic, Expression, ExpressionKind, Function, Item, ItemKind, Literal,
    MAX_DIAGNOSTICS, MatchArm, ParsedSources, Path, Pattern, RefinementKind, SourceDomainId, Span,
    StatementKind, StringLiteral, StringPart, Type, TypeKind,
};

/// Maximum namespace and reference operations performed by one resolution.
pub const MAX_RESOLUTION_WORK: usize = 4_000_000;

#[derive(Clone)]
pub(super) struct PendingItem {
    pub(super) domain: SourceDomainId,
    pub(super) path: Vec<String>,
    pub(super) module_path: Vec<String>,
    pub(super) kind: ResolvedItemKind,
    pub(super) span: Span,
    pub(super) variants: Vec<(String, Span)>,
    pub(super) public: bool,
}

pub(super) type DomainPath = (SourceDomainId, Vec<String>);
pub(super) type BoundarySpans = (Option<Span>, Option<Span>);

#[derive(Default)]
pub(super) struct ModuleInfo {
    pub(super) declarations: BTreeMap<String, ItemId>,
    pub(super) children: BTreeMap<String, ModuleId>,
    pub(super) public_children: BTreeSet<String>,
    pub(super) imports: BTreeMap<String, ItemId>,
    pub(super) exports: BTreeMap<String, ItemId>,
    pub(super) closed: bool,
}

#[derive(Clone, Copy)]
pub(super) enum Expected {
    Any,
    Type,
    Function,
    Value,
    Struct,
}

pub(super) struct Resolver<'a> {
    pub(super) parsed: &'a ParsedSources,
    pub(super) modules: Vec<ResolvedModule>,
    pub(super) module_ids: BTreeMap<DomainPath, ModuleId>,
    pub(super) module_info: Vec<ModuleInfo>,
    pub(super) items: Vec<ResolvedItem>,
    pub(super) item_ids: BTreeMap<DomainPath, ItemId>,
    pub(super) input_domains: BTreeMap<SourceDomainId, BTreeMap<String, SourceDomainId>>,
    pub(super) public_imports: BTreeSet<(ModuleId, String)>,
    pub(super) enum_variants: BTreeMap<ItemId, BTreeMap<String, u32>>,
    pub(super) references: Vec<ResolvedReference>,
    pub(super) locals: Vec<ResolvedLocal>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) work: usize,
    pub(super) exhausted: bool,
    pub(super) next_local: u32,
}

/// Resolve all names without changing the parsed syntax tree.
pub fn resolve(parsed: ParsedSources) -> Result<ResolvedProgram, Vec<Diagnostic>> {
    let mut resolver = Resolver::new(&parsed);
    resolver.collect();
    if !resolver.exhausted {
        resolver.bind_interfaces_and_imports();
        resolver.resolve_contents();
    }
    resolver.diagnostics.sort_by_key(|diagnostic| {
        (
            diagnostic.span.source_id(),
            diagnostic.span.start(),
            diagnostic.message.clone(),
        )
    });
    if resolver.diagnostics.is_empty() {
        let modules = std::mem::take(&mut resolver.modules);
        let items = std::mem::take(&mut resolver.items);
        let references = std::mem::take(&mut resolver.references);
        let locals = std::mem::take(&mut resolver.locals);
        drop(resolver);
        Ok(ResolvedProgram {
            parsed,
            modules,
            items,
            references,
            locals,
        })
    } else {
        Err(std::mem::take(&mut resolver.diagnostics))
    }
}

impl<'a> Resolver<'a> {
    pub(super) fn new(parsed: &'a ParsedSources) -> Self {
        Self {
            parsed,
            modules: Vec::new(),
            module_ids: BTreeMap::new(),
            module_info: Vec::new(),
            items: Vec::new(),
            item_ids: BTreeMap::new(),
            input_domains: BTreeMap::new(),
            public_imports: BTreeSet::new(),
            enum_variants: BTreeMap::new(),
            references: Vec::new(),
            locals: Vec::new(),
            diagnostics: Vec::new(),
            work: 0,
            exhausted: false,
            next_local: 0,
        }
    }

    pub(super) fn charge(&mut self, span: Span) -> bool {
        self.work = self.work.saturating_add(1);
        if self.work <= MAX_RESOLUTION_WORK {
            return true;
        }
        if !self.exhausted {
            self.exhausted = true;
            self.error("name resolution work limit reached", span);
        }
        false
    }

    pub(super) fn error(&mut self, message: impl Into<String>, span: Span) {
        if self.diagnostics.len() < MAX_DIAGNOSTICS {
            self.diagnostics.push(Diagnostic::error(message, span));
        }
    }

    pub(super) fn local(&mut self, span: Span) -> Option<LocalId> {
        if !self.charge(span) {
            return None;
        }
        let id = LocalId(self.next_local);
        let Some(next) = self.next_local.checked_add(1) else {
            self.error("too many local declarations", span);
            return None;
        };
        self.next_local = next;
        self.locals.push(ResolvedLocal { id, span });
        Some(id)
    }

    pub(super) fn walk_items(&self) -> Vec<LocatedItem<'a>> {
        let mut result = Vec::new();
        for source in self.parsed.iter() {
            let domain = source.domain();
            let mut stack: Vec<(&Item, Vec<String>)> = source
                .program()
                .items
                .iter()
                .rev()
                .map(|item| (item, source.module().to_vec()))
                .collect();
            while let Some((item, module_path)) = stack.pop() {
                if let ItemKind::Module(module) = &item.kind {
                    let mut child = module_path;
                    child.extend(
                        module
                            .path
                            .segments
                            .iter()
                            .map(|segment| segment.text.clone()),
                    );
                    for nested in module.items.iter().rev() {
                        stack.push((nested, child.clone()));
                    }
                } else {
                    result.push(LocatedItem {
                        item,
                        domain,
                        module_path,
                    });
                }
            }
        }
        result
    }
}

pub(super) struct LocatedItem<'a> {
    pub(super) item: &'a Item,
    pub(super) domain: SourceDomainId,
    pub(super) module_path: Vec<String>,
}

pub(super) trait SpanKey {
    fn cmp_key(&self, other: &Self) -> std::cmp::Ordering;
}

impl SpanKey for Span {
    fn cmp_key(&self, other: &Self) -> std::cmp::Ordering {
        (self.source_id(), self.start(), self.end()).cmp(&(
            other.source_id(),
            other.start(),
            other.end(),
        ))
    }
}
