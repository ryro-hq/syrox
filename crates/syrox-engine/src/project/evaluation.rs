use std::{collections::BTreeMap, mem::size_of, path::Path, sync::Arc};

use syrox_lang::{
    CanonicalType, CheckedProgram, EvaluationQueryError, EvaluationSession, ItemId, MemoId,
    PrimitiveValue, RealizedRoot, SourceDomainId, Ty, Value,
};

use super::{
    CheckConfiguration, CheckFailure, LoadedProject, ProjectOperationError, analyze, compare_lock,
    loader, map_generated_lock_error, project_plan, read_graph_lock, read_lock,
    validate_lock_policy, validate_project_limits,
};

/// A checked project whose root and transitive snapshots have been verified
/// against their Locks. Its source domains are local to this loaded graph;
/// they are not persistent recipe or Action identities.
#[derive(Debug)]
pub struct LockedProject {
    loaded: LoadedProject,
    checked: CheckedProgram,
    configuration: CheckConfiguration,
    digest: [u8; 32],
}

impl LockedProject {
    pub const fn loaded(&self) -> &LoadedProject {
        &self.loaded
    }
    pub const fn lock_digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(super) fn check_report(&self) -> super::CheckReport {
        super::CheckReport {
            path: self.loaded.path().to_path_buf(),
            declarations: self.checked.resolved().parsed().declaration_count(),
            realized_roots: 0,
        }
    }

    pub fn evaluation(&self) -> Result<ProjectEvaluation<'_>, ProjectOperationError> {
        let session = EvaluationSession::new(
            &self.checked,
            &self.configuration.policy,
            &self.configuration.environment,
            self.configuration.evaluation_limits,
        )
        .map_err(|source| CheckFailure::EvaluationSetup { source })?;
        Ok(ProjectEvaluation {
            project: self,
            session,
            catalog: None,
        })
    }
}

#[derive(Debug)]
pub struct ProjectEvaluation<'a> {
    project: &'a LockedProject,
    session: EvaluationSession<'a>,
    catalog: Option<CatalogState>,
}

type CatalogIndex = BTreeMap<String, (MemoId, Arc<CanonicalType>)>;

#[derive(Debug)]
enum CatalogState {
    Absent,
    Present(CatalogIndex),
}

impl ProjectEvaluation<'_> {
    pub fn root_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.session.root_names()
    }
    pub fn root_type(&self, name: &str) -> Option<&Ty> {
        self.session.root_type(name)
    }

    pub fn evaluate_root(&mut self, name: &str) -> Result<&RealizedRoot, ProjectOperationError> {
        self.session
            .evaluate_root(name)
            .map_err(|error| match error {
                EvaluationQueryError::UnknownRoot => ProjectOperationError::MissingOutput {
                    name: name.to_owned(),
                },
                EvaluationQueryError::InvalidSelection => invalid_set("invalid selected reference"),
                EvaluationQueryError::Setup(source) => {
                    CheckFailure::EvaluationSetup { source }.into()
                }
            })
    }

    pub fn evaluate_all(&mut self) -> Result<(), ProjectOperationError> {
        self.session
            .evaluate_all()
            .map_err(|source| CheckFailure::EvaluationSetup { source }.into())
    }

    /// Read a declared default without forcing any catalog recipe.
    pub fn default_package_id(
        &mut self,
        name: &str,
        contract: &str,
    ) -> Result<Option<String>, ProjectOperationError> {
        let Some(ty) = self.root_type(name) else {
            return Ok(None);
        };
        if !is_std_nominal(ty, &self.project.checked, &["std", "pkg", contract]) {
            return Ok(None);
        }
        let root = self
            .session
            .evaluate_root(name)
            .map_err(|error| match error {
                EvaluationQueryError::Setup(source) => {
                    CheckFailure::EvaluationSetup { source }.into()
                }
                _ => invalid_set("invalid default output"),
            })?;
        let value = root.value().ok_or_else(|| CheckFailure::Evaluation {
            input: self.project.loaded.sources().clone(),
            errors: root.diagnostics().to_vec(),
            failed_roots: vec![name.to_owned()],
        })?;
        let id = field(value, "package")
            .and_then(package_id)
            .ok_or_else(|| invalid_set("invalid default package"))?;
        let id = id.to_owned();
        self.session
            .include_root(name)
            .map_err(|error| match error {
                EvaluationQueryError::Setup(source) => {
                    CheckFailure::EvaluationSetup { source }.into()
                }
                _ => invalid_set("invalid default output"),
            })?;
        Ok(Some(id))
    }

    /// Enumerate a checked `std::PackageSet` output without calling its factories.
    /// `None` means the named output does not have the package-set contract.
    pub fn package_names(
        &mut self,
        name: &str,
    ) -> Result<Option<Vec<String>>, ProjectOperationError> {
        if name == "packages" {
            self.catalog()?;
            return Ok(self
                .catalog_entries()
                .map(|entries| entries.keys().cloned().collect()));
        }
        Ok(self
            .package_entries(name)?
            .map(|entries| entries.into_iter().map(|(key, _)| key.to_owned()).collect()))
    }

    /// Select an existing `RecipeRef` without invoking its memoized factory.
    /// The returned value is borrowed from this evaluation operation so its
    /// identity remains shared with any named reference to the same recipe.
    /// `None` means the output is not a set or the key is absent.
    pub fn package_reference(
        &mut self,
        name: &str,
        key: &str,
    ) -> Result<Option<&Value>, ProjectOperationError> {
        Ok(self
            .package_entries(name)?
            .and_then(|entries| entries.into_iter().find(|(name, _)| *name == key))
            .map(|(_, reference)| reference))
    }

    /// Force one catalog recipe and prepare only that entry for Plan projection.
    /// Returns false when the project does not expose a `PackageSet` output.
    pub fn select_package(&mut self, key: &str) -> Result<bool, ProjectOperationError> {
        if self.session.selected_roots().any(|root| root.name() == key) {
            return Ok(true);
        }
        self.catalog()?;
        let Some(entries) = self.catalog_entries() else {
            return Ok(false);
        };
        let (id, result) = entries
            .get(key)
            .ok_or_else(|| ProjectOperationError::MissingPackageReference {
                key: key.to_owned(),
            })?
            .clone();
        self.session
            .force_reference("packages", key, &id, result)
            .map_err(|error| match error {
                EvaluationQueryError::Setup(source) => {
                    CheckFailure::EvaluationSetup { source }.into()
                }
                _ => invalid_set("invalid selected reference"),
            })?;
        Ok(true)
    }

    /// Select every entry of the typed catalog in one evaluation operation.
    /// Discovery stays lazy; this explicitly requested Plan forces each recipe.
    pub fn select_all_packages(&mut self) -> Result<bool, ProjectOperationError> {
        let Some(names) = self.package_names("packages")? else {
            return Ok(false);
        };
        for name in names {
            self.select_package(&name)?;
        }
        Ok(true)
    }

    /// A full Plan includes ordinary outputs alongside selected set entries.
    /// Ephemeral references are inputs to selection, not serializable roots.
    pub fn include_project_outputs(&mut self) -> Result<(), ProjectOperationError> {
        let names: Vec<_> = self
            .root_names()
            .filter(|name| {
                *name != "packages"
                    && self.root_type(name).is_some_and(|ty| {
                        !is_package_set_type(ty, &self.project.checked)
                            && !is_transient_reference_type(ty, &self.project.checked)
                    })
            })
            .map(str::to_owned)
            .collect();
        for name in names {
            self.session
                .include_root(&name)
                .map_err(|error| match error {
                    EvaluationQueryError::Setup(source) => {
                        CheckFailure::EvaluationSetup { source }.into()
                    }
                    _ => invalid_set("invalid default output"),
                })?;
        }
        Ok(())
    }

    pub fn into_selected_plan(self) -> Result<crate::Plan, ProjectOperationError> {
        let mut evaluation = self;
        let mut cursor = 0;
        let mut edges = 0_usize;
        while cursor < evaluation.session.selected_roots().len() {
            let required = {
                let root = evaluation
                    .session
                    .selected_roots()
                    .nth(cursor)
                    .expect("bounded selected root");
                let value = root.value().ok_or_else(|| CheckFailure::Evaluation {
                    input: evaluation.project.loaded.sources().clone(),
                    errors: root.diagnostics().to_vec(),
                    failed_roots: vec![root.name().to_owned()],
                })?;
                required_packages(value)?
            };
            edges = edges.saturating_add(required.len());
            if edges > crate::MAX_PLAN_PACKAGE_EDGES {
                return Err(invalid_set("selected dependencies exceed limit"));
            }
            for key in required {
                evaluation.catalog()?;
                let id = evaluation
                    .catalog_entries()
                    .and_then(|entries| entries.get(&key))
                    .ok_or_else(|| invalid_set("selected dependency is missing from package set"))?
                    .0
                    .clone();
                if !evaluation.select_package(&key)? {
                    return Err(invalid_set("selected dependency requires a package set"));
                }
                if evaluation.session.selected_roots().len() > crate::MAX_PLAN_PACKAGE_NODES {
                    return Err(invalid_set("selected package count exceeds limit"));
                }
                // Aliases may reuse an existing root. Validate the recipe
                // selected by this key, not whichever root happened to be last.
                let selected = evaluation
                    .session
                    .selected_reference(&id)
                    .ok_or_else(|| invalid_set("selected reference has no evaluated recipe"))?;
                let selected_value = selected.value().ok_or_else(|| CheckFailure::Evaluation {
                    input: evaluation.project.loaded.sources().clone(),
                    errors: selected.diagnostics().to_vec(),
                    failed_roots: vec![selected.name().to_owned()],
                })?;
                let selected_id = recipe_package_id(selected_value);
                if selected_id != Some(key.as_str()) {
                    return Err(invalid_set("dependency key does not match package id"));
                }
            }
            cursor += 1;
        }
        let realized = evaluation
            .session
            .into_selected()
            .map_err(|source| CheckFailure::EvaluationSetup { source })?;
        project_plan(
            &evaluation.project.loaded,
            &realized,
            evaluation.project.digest,
            &evaluation.project.configuration,
        )
    }

    fn package_entries(
        &mut self,
        name: &str,
    ) -> Result<Option<Vec<(&str, &Value)>>, ProjectOperationError> {
        let Some(ty) = self.root_type(name) else {
            return Ok(None);
        };
        if !is_package_set_type(ty, &self.project.checked) {
            return Ok(None);
        }
        let root = self
            .session
            .evaluate_root(name)
            .map_err(|error| match error {
                EvaluationQueryError::UnknownRoot => ProjectOperationError::MissingOutput {
                    name: name.to_owned(),
                },
                EvaluationQueryError::InvalidSelection => invalid_set("invalid selected reference"),
                EvaluationQueryError::Setup(source) => {
                    CheckFailure::EvaluationSetup { source }.into()
                }
            })?;
        let Some(value) = root.value() else {
            return Err(CheckFailure::Evaluation {
                input: self.project.loaded.sources().clone(),
                errors: root.diagnostics().to_vec(),
                failed_roots: vec![name.to_owned()],
            }
            .into());
        };
        Ok(Some(package_set_entries(value)?))
    }

    /// Validate the set once per operation and retain only its keys and memo
    /// handles. The original values stay in the evaluation session.
    fn catalog(&mut self) -> Result<(), ProjectOperationError> {
        if self.catalog.is_some() {
            return Ok(());
        }
        let limit = self
            .project
            .configuration
            .evaluation_limits
            .max_retained_expansion_bytes;
        let Some(entries) = self.package_entries("packages")? else {
            self.catalog = Some(CatalogState::Absent);
            return Ok(());
        };
        let mut index = BTreeMap::new();
        let mut retained = 0_usize;
        for (key, reference) in entries {
            let Some(Value::MemoizedFunction { ty, id }) = field(reference, "factory") else {
                return Err(invalid_set("invalid recipe reference factory"));
            };
            let CanonicalType::Function {
                parameters,
                result,
                once: false,
            } = ty.as_ref()
            else {
                return Err(invalid_set("invalid recipe factory type"));
            };
            if !parameters.is_empty() {
                return Err(invalid_set("invalid recipe factory parameters"));
            }
            let bytes = key
                .len()
                .saturating_add(size_of::<(String, MemoId, Arc<CanonicalType>)>())
                .saturating_add(6 * size_of::<usize>());
            retained = retained.saturating_add(bytes);
            if retained > limit {
                return Err(CheckFailure::EvaluationSetup {
                    source: syrox_lang::EvaluationSetupError::EvaluationRetainedExpansionLimit,
                }
                .into());
            }
            index.insert(key.to_owned(), (id.clone(), result.clone()));
        }
        self.session
            .reserve_metadata_bytes(retained)
            .map_err(|source| CheckFailure::EvaluationSetup { source })?;
        self.catalog = Some(CatalogState::Present(index));
        Ok(())
    }

    fn catalog_entries(&self) -> Option<&CatalogIndex> {
        match self.catalog.as_ref()? {
            CatalogState::Absent => None,
            CatalogState::Present(entries) => Some(entries),
        }
    }

    pub fn into_plan(self) -> Result<crate::Plan, ProjectOperationError> {
        let realized = self
            .session
            .into_realized()
            .map_err(|source| CheckFailure::EvaluationSetup { source })?;
        project_plan(
            &self.project.loaded,
            &realized,
            self.project.digest,
            &self.project.configuration,
        )
    }
}

fn is_package_set_type(ty: &Ty, program: &CheckedProgram) -> bool {
    let is_std = |id: ItemId, path: &[&str]| {
        program.resolved().items().any(|item| {
            item.id() == id
                && item.domain() == SourceDomainId::standard_library()
                && item
                    .path()
                    .segments()
                    .iter()
                    .map(String::as_str)
                    .eq(path.iter().copied())
        })
    };
    let set = |ty: &Ty| matches!(ty, Ty::Specialization { template, arguments } if arguments.len() == 1 && is_std(*template, &["std", "catalog", "PackageSet"]));
    set(ty)
        || matches!(ty, Ty::Specialization { template, arguments }
            if arguments.len() == 2 && is_std(*template, &["std", "result", "Result"])
                && set(&arguments[0]) && matches!(&arguments[1], Ty::Nominal(key) if is_std(*key, &["std", "maps", "MapKey"])))
}

fn is_transient_reference_type(ty: &Ty, program: &CheckedProgram) -> bool {
    matches!(ty, Ty::Specialization { template, arguments } if arguments.len() == 1
    && program.resolved().items().any(|item| {
        item.id() == *template
            && item.domain() == SourceDomainId::standard_library()
                && ["RecipeRef", "OutputRef"].contains(&item.path().segments().last().map_or("", String::as_str))
            && item.path().segments().iter().take(2).map(String::as_str).eq(["std", "recipe"])
    }))
}

fn is_std_nominal(ty: &Ty, program: &CheckedProgram, path: &[&str]) -> bool {
    matches!(ty, Ty::Nominal(id) if program.resolved().items().any(|item| {
        item.id() == *id && item.domain() == SourceDomainId::standard_library()
            && item.path().segments().iter().map(String::as_str).eq(path.iter().copied())
    }))
}

fn package_set_entries(value: &Value) -> Result<Vec<(&str, &Value)>, ProjectOperationError> {
    let value = if let Value::Variant { ty, index, payload } = value {
        if !std_specialization(ty, &["std", "result", "Result"], 2) {
            return Err(invalid_set("expected a std::Result"));
        }
        match (*index, payload.as_slice()) {
            (0, [set]) => set,
            (1, [_]) => return Err(invalid_set("duplicate catalog key")),
            _ => return Err(invalid_set("invalid result variant")),
        }
    } else {
        value
    };
    let Value::Struct { ty, fields, .. } = value else {
        return Err(invalid_set("expected a PackageSet"));
    };
    if !std_specialization(ty, &["std", "catalog", "PackageSet"], 1)
        || !matches!(fields.as_slice(), [(name, _)] if name == "factories")
    {
        return Err(invalid_set("invalid PackageSet fields"));
    }
    let Value::Struct {
        ty: map_ty, fields, ..
    } = &fields[0].1
    else {
        return Err(invalid_set("expected an ordered map"));
    };
    if !std_specialization(map_ty, &["std", "maps", "OrderedMap"], 1) {
        return Err(invalid_set("invalid ordered map type"));
    }
    let [
        (
            name,
            Value::List {
                ty: entries_ty,
                items,
            },
        ),
    ] = fields.as_slice()
    else {
        return Err(invalid_set("invalid ordered map fields"));
    };
    if !matches!(entries_ty.as_ref(), CanonicalType::List(entry)
        if std_specialization(entry, &["std", "maps", "MapEntry"], 1))
    {
        return Err(invalid_set("invalid map entry list type"));
    }
    if name != "entries" || items.len() > crate::MAX_PLAN_PACKAGE_NODES {
        return Err(invalid_set("invalid catalog entry count"));
    }
    let mut entries = Vec::with_capacity(items.len());
    let mut bytes = 0_usize;
    for item in items {
        let (key, reference) = package_entry(item)?;
        bytes = bytes.saturating_add(key.len());
        if bytes > crate::MAX_PLAN_RETAINED_BYTES
            || entries.last().is_some_and(|(last, _)| *last >= key)
        {
            return Err(invalid_set("catalog keys exceed limits or are not ordered"));
        }
        entries.push((key, reference));
    }
    Ok(entries)
}

fn package_entry(item: &Value) -> Result<(&str, &Value), ProjectOperationError> {
    let Value::Variant {
        ty: entry_ty,
        index: 0,
        payload,
    } = item
    else {
        return Err(invalid_set("invalid map entry"));
    };
    if !std_specialization(entry_ty, &["std", "maps", "MapEntry"], 1) {
        return Err(invalid_set("invalid map entry type"));
    }
    let [
        Value::Nominal {
            ty: key_ty,
            value: PrimitiveValue::Str(key),
            resource: false,
            ..
        },
        reference @ Value::Struct {
            ty: reference_ty,
            fields: reference_fields,
            ..
        },
    ] = payload.as_slice()
    else {
        return Err(invalid_set("invalid catalog key or reference"));
    };
    if !matches!(key_ty.as_ref(), CanonicalType::Nominal(identity)
        if identity.domain() == SourceDomainId::standard_library()
            && identity.path() == ["std", "maps", "MapKey"])
        || !std_specialization(reference_ty, &["std", "recipe", "RecipeRef"], 1)
    {
        return Err(invalid_set("invalid catalog key or reference type"));
    }
    if !matches!(reference_fields.as_slice(), [(name, Value::MemoizedFunction { .. })] if name == "factory")
    {
        return Err(invalid_set("invalid recipe reference factory"));
    }
    Ok((key, reference))
}

fn std_specialization(ty: &CanonicalType, path: &[&str], arity: usize) -> bool {
    matches!(ty, CanonicalType::Specialization { template, arguments }
        if template.domain() == SourceDomainId::standard_library()
            && template.path().iter().map(String::as_str).eq(path.iter().copied())
            && arguments.len() == arity)
}

fn invalid_set(reason: &'static str) -> ProjectOperationError {
    ProjectOperationError::InvalidPackageSet { reason }
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Struct { fields, .. } = value else {
        return None;
    };
    fields
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value)
}

fn package_id(value: &Value) -> Option<&str> {
    let Value::Nominal {
        ty,
        value: PrimitiveValue::Str(id),
        resource: false,
    } = value
    else {
        return None;
    };
    matches!(ty.as_ref(), CanonicalType::Nominal(identity)
        if identity.domain() == SourceDomainId::standard_library()
            && identity.path() == ["std", "pkg", "PackageId"])
    .then_some(id)
}

fn recipe_package_id(value: &Value) -> Option<&str> {
    let package = field(value, "package")
        .filter(|package| matches!(package, Value::Struct { .. }))
        .unwrap_or(value);
    field(package, "id").and_then(package_id)
}

fn optional_component(value: &Value) -> Option<&Value> {
    match value {
        Value::Variant {
            ty,
            index: 1,
            payload,
        } if std_specialization(ty, &["std", "option", "Option"], 1) => payload.first(),
        Value::Variant { ty, index: 0, .. }
            if std_specialization(ty, &["std", "option", "Option"], 1) =>
        {
            None
        }
        value => Some(value),
    }
}

fn required_packages(value: &Value) -> Result<Vec<String>, ProjectOperationError> {
    let mut required = Vec::new();
    if let Some(package) = field(value, "package")
        .filter(|package| matches!(package, Value::Struct { .. }))
        .or_else(|| field(value, "dependencies").map(|_| value))
    {
        let Some(Value::List { items, .. }) = field(package, "dependencies") else {
            return Err(invalid_set("invalid recipe dependencies"));
        };
        for dependency in items {
            let id = field(dependency, "package")
                .and_then(package_id)
                .ok_or_else(|| invalid_set("invalid recipe dependency"))?;
            required.push(id.to_owned());
        }
    }
    if let Some(inputs) = field(value, "build_inputs").and_then(optional_component) {
        let Some(Value::List { items, .. }) = field(inputs, "selected") else {
            return Err(invalid_set("invalid build inputs"));
        };
        for input in items {
            let id = field(input, "package")
                .and_then(package_id)
                .ok_or_else(|| invalid_set("invalid build input provider"))?;
            required.push(id.to_owned());
        }
    }
    if let Some(application) = field(value, "application").and_then(optional_component) {
        for name in ["loader", "libraries"] {
            let Some(Value::List { items, .. }) = field(application, name) else {
                return Err(invalid_set("invalid runtime providers"));
            };
            for provider in items {
                let id = field(provider, "package")
                    .and_then(package_id)
                    .ok_or_else(|| invalid_set("invalid runtime provider"))?;
                required.push(id.to_owned());
            }
        }
    }
    if required.len() > crate::MAX_PLAN_PACKAGE_EDGES {
        return Err(invalid_set("selected dependencies exceed limit"));
    }
    required.sort();
    required.dedup();
    Ok(required)
}

/// Load once, authenticate the complete graph, and check every source. No
/// output expression or recipe factory is evaluated until a query is made.
pub fn open_locked_project_with(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<LockedProject, ProjectOperationError> {
    validate_project_limits(configuration.project_limits)?;
    validate_lock_policy(configuration)?;
    let loaded = loader::load_project_linux(path, configuration)?;
    let expected =
        crate::lock::LockManifest::generate(&loaded, configuration.standard_library.as_ref())
            .map_err(map_generated_lock_error)?;
    let actual = read_lock(&loaded)?.ok_or(ProjectOperationError::MissingLock {
        name: crate::LOCK_FILE_NAME,
    })?;
    compare_lock(&actual, &expected)?;
    if read_graph_lock(&loaded)?.is_some() {
        return Err(ProjectOperationError::GraphDrift);
    }
    let checked = analyze::check_loaded(&loaded, configuration)?;
    Ok(LockedProject {
        loaded,
        checked,
        configuration: configuration.clone(),
        digest: *actual.digest(),
    })
}
