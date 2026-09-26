use std::collections::BTreeMap;
use std::sync::Arc;

use thiserror::Error;

use crate::{MAX_SOURCE_BYTES, MAX_SOURCES};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceDomainId(u32);

impl SourceDomainId {
    pub(crate) const PROJECT: Self = Self(0);
    pub(crate) const STANDARD_LIBRARY: Self = Self(1);

    /// The domain shared by ordinary project sources.
    pub const fn project() -> Self {
        Self::PROJECT
    }

    /// The reserved domain assigned only by a caller asserting authenticated std text.
    pub const fn standard_library() -> Self {
        Self::STANDARD_LIBRARY
    }

    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceId(u32);

impl SourceId {
    pub(crate) const SINGLE: Self = Self(0);

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Span {
    pub(crate) source: SourceId,
    pub(crate) start: u32,
    pub(crate) end: u32,
}

impl Span {
    pub(crate) const fn new(source: SourceId, start: u32, end: u32) -> Self {
        Self { source, start, end }
    }

    #[must_use]
    pub(crate) const fn join(self, other: Self) -> Self {
        assert!(
            self.source.0 == other.source.0,
            "cannot join spans from different sources"
        );
        Self::new(self.source, self.start, other.end)
    }

    pub const fn source_id(self) -> SourceId {
        self.source
    }

    pub const fn start(self) -> u32 {
        self.start
    }

    pub const fn end(self) -> u32 {
        self.end
    }

    pub(crate) fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

#[derive(Clone, Debug)]
pub struct Source {
    name: Arc<str>,
    text: Arc<str>,
}

impl Source {
    pub fn new(name: impl Into<Arc<str>>, text: impl Into<Arc<str>>) -> Result<Self, SourceError> {
        let name = name.into();
        let text = text.into();
        if text.len() > MAX_SOURCE_BYTES || text.len() > u32::MAX as usize {
            return Err(SourceError::TooLarge);
        }
        Ok(Self { name, text })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Debug)]
pub struct SourceSet {
    sources: Vec<Source>,
    domains: Vec<SourceDomainId>,
    input_domains: Option<Arc<BTreeMap<String, SourceDomainId>>>,
    total_bytes: usize,
    next_domain: u32,
}

impl Default for SourceSet {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceSet {
    pub const fn new() -> Self {
        Self {
            sources: Vec::new(),
            domains: Vec::new(),
            input_domains: None,
            total_bytes: 0,
            next_domain: 2,
        }
    }

    pub fn add(
        &mut self,
        name: impl Into<Arc<str>>,
        text: impl Into<Arc<str>>,
    ) -> Result<SourceId, SourceError> {
        self.insert(Source::new(name, text)?)
    }

    pub fn insert(&mut self, source: Source) -> Result<SourceId, SourceError> {
        self.insert_in_domain(SourceDomainId::PROJECT, source)
    }

    /// Creates a distinct sealed input source domain bound to a trusted project alias.
    pub fn create_input_domain(
        &mut self,
        alias: impl Into<String>,
    ) -> Result<SourceDomainId, SourceError> {
        let alias = alias.into();
        if alias == "std" {
            return Err(SourceError::ReservedInputAlias);
        }
        if self
            .input_domains
            .as_ref()
            .is_some_and(|domains| domains.contains_key(&alias))
        {
            return Err(SourceError::DuplicateInputAlias);
        }
        let domain = SourceDomainId(self.next_domain);
        self.next_domain = self
            .next_domain
            .checked_add(1)
            .ok_or(SourceError::TooManyDomains)?;
        Arc::make_mut(
            self.input_domains
                .get_or_insert_with(|| Arc::new(BTreeMap::new())),
        )
        .insert(alias, domain);
        Ok(domain)
    }

    pub fn add_to_input_domain(
        &mut self,
        domain: SourceDomainId,
        name: impl Into<Arc<str>>,
        text: impl Into<Arc<str>>,
    ) -> Result<SourceId, SourceError> {
        if domain.0 < 2 || domain.0 >= self.next_domain {
            return Err(SourceError::InvalidInputDomain);
        }
        let source = Source::new(name, text)?;
        self.insert_in_domain(domain, source)
    }

    /// Adds a source which the caller asserts is authenticated standard-library text.
    ///
    /// This public call is a trusted boundary: the caller must authenticate and
    /// pin the source before assigning the reserved origin. The domain records
    /// that assertion but does not authenticate bytes itself. Source spelling
    /// alone must never select this domain.
    pub fn add_standard_library(
        &mut self,
        name: impl Into<Arc<str>>,
        text: impl Into<Arc<str>>,
    ) -> Result<SourceId, SourceError> {
        let source = Source::new(name, text)?;
        self.insert_in_domain(SourceDomainId::STANDARD_LIBRARY, source)
    }

    fn insert_in_domain(
        &mut self,
        domain: SourceDomainId,
        source: Source,
    ) -> Result<SourceId, SourceError> {
        if self.sources.len() >= MAX_SOURCES {
            return Err(SourceError::TooManySources);
        }
        let total_bytes = self
            .total_bytes
            .checked_add(source.text.len())
            .ok_or(SourceError::SourceSetTooLarge)?;
        let id =
            SourceId(u32::try_from(self.sources.len()).map_err(|_| SourceError::TooManySources)?);
        self.sources.push(source);
        self.domains.push(domain);
        self.total_bytes = total_bytes;
        Ok(id)
    }

    pub fn get(&self, id: SourceId) -> Option<&Source> {
        self.sources.get(id.index())
    }

    pub fn domain(&self, id: SourceId) -> Option<SourceDomainId> {
        self.domains.get(id.index()).copied()
    }

    pub(crate) fn input_domains(&self) -> Option<&BTreeMap<String, SourceDomainId>> {
        self.input_domains.as_deref()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (SourceId, &Source)> {
        self.sources.iter().enumerate().map(|(index, source)| {
            let index = u32::try_from(index).expect("source count is bounded by u32");
            (SourceId(index), source)
        })
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SourceError {
    #[error("source exceeds the {MAX_SOURCE_BYTES}-byte limit")]
    TooLarge,
    #[error("source set exceeds the {MAX_SOURCES}-source limit")]
    TooManySources,
    #[error("source set byte count cannot be represented")]
    SourceSetTooLarge,
    #[error("source domain count cannot be represented")]
    TooManyDomains,
    #[error("input source domain was not created by this source set")]
    InvalidInputDomain,
    #[error("input alias is already bound in this source set")]
    DuplicateInputAlias,
    #[error("`std` is reserved and cannot be used as an input alias")]
    ReservedInputAlias,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ids_are_stable_and_source_count_is_bounded() {
        let mut sources = SourceSet::new();
        let first = sources.add("first.srx", "").unwrap();
        let second = sources.add("second.srx", "").unwrap();

        assert_eq!(sources.get(first).unwrap().name(), "first.srx");
        assert_eq!(sources.get(second).unwrap().name(), "second.srx");

        for index in sources.len()..MAX_SOURCES {
            sources.add(index.to_string(), "").unwrap();
        }
        assert_eq!(
            sources.add("too-many.srx", "").unwrap_err(),
            SourceError::TooManySources
        );
    }

    #[test]
    fn source_domains_are_explicit_and_project_is_the_default() {
        let mut sources = SourceSet::new();
        let project = sources.add("project.srx", "").unwrap();
        let input_domain = sources.create_input_domain("input").unwrap();
        let input = sources
            .add_to_input_domain(input_domain, "input.srx", "")
            .unwrap();
        let standard = sources.add_standard_library("std.srx", "").unwrap();

        assert_eq!(sources.domain(project), Some(SourceDomainId::PROJECT));
        assert_eq!(sources.domain(input), Some(input_domain));
        assert_eq!(
            sources.domain(standard),
            Some(SourceDomainId::STANDARD_LIBRARY)
        );
    }

    #[test]
    fn input_aliases_are_bound_when_their_domains_are_created() {
        let mut sources = SourceSet::new();
        let domain = sources.create_input_domain("catalog").unwrap();

        assert_eq!(
            sources.input_domains().unwrap().get("catalog"),
            Some(&domain)
        );
        assert_eq!(
            sources.create_input_domain("catalog").unwrap_err(),
            SourceError::DuplicateInputAlias
        );
        assert_eq!(
            sources.create_input_domain("std").unwrap_err(),
            SourceError::ReservedInputAlias
        );
    }
}
