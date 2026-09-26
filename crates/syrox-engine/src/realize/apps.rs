use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use super::{
    BuildProgress, RealizeError, ResolvedBuild, realize_declared_application, reference_parts,
};
use crate::{
    BuildCancellation, CheckConfiguration, Plan, PlanApplication, PlanPackageId, RuntimeClosure,
    RuntimeError, RuntimeOutput, RuntimeRequest, Store, StoreError, UserConfiguration,
    plan_project_with, verify_runtime_with_cancellation,
};

#[derive(Debug)]
pub struct ResolvedApplication {
    project: PathBuf,
    export: String,
    plan: Arc<Plan>,
    application: PlanApplication,
}

impl ResolvedApplication {
    pub fn project(&self) -> &Path {
        &self.project
    }
    pub fn export(&self) -> &str {
        &self.export
    }
    pub const fn description(&self) -> &PlanApplication {
        &self.application
    }

    fn build(&self, id: &PlanPackageId) -> ResolvedBuild {
        let package = self
            .plan
            .packages()
            .find(|package| package.id() == id)
            .expect("validated application package");
        let build = self
            .plan
            .builds()
            .find(|build| build.package() == id)
            .expect("validated application build");
        let mut lock_digest = String::new();
        for byte in self.plan.lock_digest() {
            write!(lock_digest, "{byte:02x}").expect("String write is infallible");
        }
        ResolvedBuild {
            project: self.project.clone(),
            export: package.export().unwrap_or(id.as_str()).to_owned(),
            lock_digest,
            plan: self.plan.clone(),
            build: build.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ApplicationError {
    #[error("project has no std::DefaultApplication; select a public application export")]
    MissingDefault,
    #[error("no declared application for export `{0}`")]
    MissingApplication(String),
    #[error(transparent)]
    Resolve(#[from] RealizeError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Project(#[from] crate::ProjectOperationError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Pure selection of a locked application; no build or Store access.
pub fn resolve_application(
    reference: &str,
    user: &UserConfiguration,
    checks: &CheckConfiguration,
) -> Result<ResolvedApplication, ApplicationError> {
    let (project, export, pin) = reference_parts(reference, user)?;
    let project = std::path::absolute(project)?;
    let plan = plan_project_with(&project, checks)?;
    if pin.is_some_and(|pin| pin.as_bytes() != plan.lock_digest()) {
        return Err(RealizeError::CatalogDrift.into());
    }
    let package = if let Some(export) = export {
        plan.packages()
            .find(|package| package.export() == Some(export))
            .ok_or_else(|| ApplicationError::MissingApplication(export.to_owned()))?
    } else {
        let default = plan
            .default_application()
            .ok_or(ApplicationError::MissingDefault)?;
        plan.packages()
            .find(|package| package.id() == default)
            .expect("validated default application")
    };
    let application = plan
        .applications()
        .find(|app| app.package() == package.id())
        .ok_or_else(|| {
            ApplicationError::MissingApplication(
                package.export().unwrap_or(package.id().as_str()).to_owned(),
            )
        })?
        .clone();
    let export = package
        .export()
        .expect("validated application export")
        .to_owned();
    Ok(ResolvedApplication {
        project,
        export,
        plan: Arc::new(plan),
        application,
    })
}

/// Realize precisely the declared buildable output roles, then verify the
/// closure before the CLI may start a runtime session.
pub fn realize_application(
    resolved: &ResolvedApplication,
    user: &UserConfiguration,
    worker: &Path,
    offline: bool,
    cancellation: &BuildCancellation,
    mut progress: impl FnMut(&PlanPackageId, BuildProgress),
) -> Result<RuntimeClosure, ApplicationError> {
    let build = resolved.build(resolved.application.package());
    let realized =
        realize_declared_application(&build, user, worker, offline, cancellation, &mut progress)?;
    let application = RuntimeOutput {
        root: realized.result.root,
        receipt: realized.result.receipt,
    };
    cancellation.check().map_err(RealizeError::from)?;
    let store = Store::initialize(user.store())?;
    let request = RuntimeRequest {
        application,
        loader: realized.loader,
        libraries: realized.libraries,
    };
    let closure = verify_runtime_with_cancellation(&store, &request, cancellation)?;
    if cancellation.is_cancelled() {
        return Err(RuntimeError::Cancelled.into());
    }
    Ok(closure)
}
