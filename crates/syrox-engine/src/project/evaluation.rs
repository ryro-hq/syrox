use std::path::Path;

use syrox_lang::{CheckedProgram, EvaluationQueryError, EvaluationSession, RealizedRoot, Ty};

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
        })
    }
}

#[derive(Debug)]
pub struct ProjectEvaluation<'a> {
    project: &'a LockedProject,
    session: EvaluationSession<'a>,
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
