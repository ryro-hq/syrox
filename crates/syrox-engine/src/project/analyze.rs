use std::path::Path;

use syrox_lang::{
    RealizedRootOutcome, SourceSet, check, evaluate_with_limits, parse_sources, resolve,
};

use super::{CheckConfiguration, CheckFailure, CheckReport, LoadedProject, ValidatedProject};

pub(super) fn validate_loaded(
    loaded: LoadedProject,
    configuration: &CheckConfiguration,
) -> Result<ValidatedProject, CheckFailure> {
    let input = &loaded.sources;
    let parsed = parse_sources(input).map_err(|errors| CheckFailure::Diagnostics {
        input: input.clone(),
        errors,
    })?;
    let declarations = parsed.declaration_count();
    let resolved = resolve(parsed).map_err(|errors| CheckFailure::Diagnostics {
        input: input.clone(),
        errors,
    })?;
    let checked =
        check(resolved, &configuration.policy).map_err(|errors| CheckFailure::Diagnostics {
            input: input.clone(),
            errors,
        })?;
    let realized = evaluate_with_limits(
        &checked,
        &configuration.policy,
        &configuration.environment,
        configuration.evaluation_limits,
    )
    .map_err(|source| CheckFailure::EvaluationSetup { source })?;
    if !realized.is_success() {
        let errors = realized.diagnostics().cloned().collect();
        let failed_roots = realized
            .roots()
            .filter(|root| matches!(root.outcome(), RealizedRootOutcome::Failed(_)))
            .map(|root| root.name().to_owned())
            .collect();
        return Err(CheckFailure::Evaluation {
            input: input.clone(),
            errors,
            failed_roots,
        });
    }
    Ok(ValidatedProject {
        loaded,
        declarations,
        realized,
    })
}

pub(super) fn check_sources(
    path: &Path,
    input: SourceSet,
    configuration: &CheckConfiguration,
) -> Result<CheckReport, CheckFailure> {
    let parsed = parse_sources(&input).map_err(|errors| CheckFailure::Diagnostics {
        input: input.clone(),
        errors,
    })?;
    let declarations = parsed.declaration_count();
    let resolved = resolve(parsed).map_err(|errors| CheckFailure::Diagnostics {
        input: input.clone(),
        errors,
    })?;
    let checked =
        check(resolved, &configuration.policy).map_err(|errors| CheckFailure::Diagnostics {
            input: input.clone(),
            errors,
        })?;
    let realized = evaluate_with_limits(
        &checked,
        &configuration.policy,
        &configuration.environment,
        configuration.evaluation_limits,
    )
    .map_err(|source| CheckFailure::EvaluationSetup { source })?;
    let realized_roots = realized.roots().len();
    if !realized.is_success() {
        let errors = realized.diagnostics().cloned().collect();
        let failed_roots = realized
            .roots()
            .filter(|root| matches!(root.outcome(), RealizedRootOutcome::Failed(_)))
            .map(|root| root.name().to_owned())
            .collect();
        return Err(CheckFailure::Evaluation {
            input,
            errors,
            failed_roots,
        });
    }
    Ok(CheckReport {
        path: path.to_path_buf(),
        declarations,
        realized_roots,
    })
}
