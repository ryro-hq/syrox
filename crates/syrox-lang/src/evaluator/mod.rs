//! Pure, bounded evaluation of checked Syrox programs.

mod environment;
mod index;
mod model;
mod runtime;
mod types;

#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, mem::size_of};

use crate::{CheckPolicy, CheckedProgram, ItemKind, OutputKind, Span};

pub use environment::{EnvironmentError, EvaluationEnvironment, EvaluationSetupError};
pub use model::{
    CanonicalType, EvaluationLimit, EvaluationLimits, GoverningScope, MAX_EVALUATION_DEPTH,
    MAX_EVALUATION_EXPANSION, MAX_EVALUATION_RETAINED_EXPANSION, MAX_EVALUATION_SETUP_BYTES,
    MAX_EVALUATION_SETUP_STEPS, MAX_EVALUATION_STEPS, MAX_EVALUATION_TOTAL_STEPS, PrimitiveValue,
    RealizedProgram, RealizedRoot, RealizedRootOutcome, ResourceClaim, ResourceClaimKey, Value,
};

use index::ProgramIndex;
use runtime::Evaluator;

pub fn evaluate(
    program: &CheckedProgram,
    policy: &CheckPolicy,
    environment: &EvaluationEnvironment,
) -> Result<RealizedProgram, EvaluationSetupError> {
    evaluate_with_limits(program, policy, environment, EvaluationLimits::default())
}

pub fn evaluate_with_limits(
    program: &CheckedProgram,
    policy: &CheckPolicy,
    environment: &EvaluationEnvironment,
    limits: EvaluationLimits,
) -> Result<RealizedProgram, EvaluationSetupError> {
    validate_limits(limits)?;
    program
        .require_policy(policy)
        .map_err(|mismatch| EvaluationSetupError::PolicyMismatch {
            checked: mismatch.checked,
            supplied: mismatch.supplied,
        })?;
    if &environment.policy != policy {
        return Err(EvaluationSetupError::EnvironmentPolicyMismatch);
    }
    if environment.predicates.len() != policy.predicates().len()
        || policy
            .predicates()
            .any(|rule| !environment.predicates.contains_key(&rule.name))
    {
        return Err(EvaluationSetupError::MissingPredicateCallback);
    }

    let index = ProgramIndex::new(program, policy, limits)?;
    let mut roots = Vec::new();
    let mut diagnostics_used = 0_usize;
    let mut total_steps = 0_usize;
    let mut retained_expansion = 0_usize;
    let mut shared_outputs = BTreeMap::new();
    for source in program.resolved().parsed().iter() {
        if source.domain() != crate::SourceDomainId::project() {
            continue;
        }
        for item in &source.program().items {
            let ItemKind::Outputs(outputs) = &item.kind else {
                continue;
            };
            for output in &outputs.entries {
                let OutputKind::Value { name, value, .. } = &output.kind else {
                    continue;
                };
                retained_expansion = retained_expansion.saturating_add(size_of::<RealizedRoot>());
                if retained_expansion > limits.max_retained_expansion_bytes {
                    return Err(EvaluationSetupError::EvaluationRetainedExpansionLimit);
                }
                let remaining_bytes = limits.max_retained_expansion_bytes - retained_expansion;
                let root_limits = EvaluationLimits {
                    max_total_steps: limits.max_total_steps.saturating_sub(total_steps),
                    max_retained_expansion_bytes: remaining_bytes,
                    max_diagnostics: limits.max_diagnostics.saturating_sub(diagnostics_used),
                    ..limits
                };
                let root = index
                    .identity_at(output.span)
                    .ok_or(EvaluationSetupError::MissingRootIdentity)?;
                let mut evaluator = Evaluator::new(
                    program,
                    policy,
                    environment,
                    &index,
                    root_limits,
                    value.span,
                    root.clone(),
                );
                evaluator.inherit_outputs(std::mem::take(&mut shared_outputs));
                let ty = evaluator.canonical_expression_type(value);
                if let Some(error) = evaluator.operation_error {
                    return Err(error);
                }
                let evaluated = evaluator
                    .expand(name.text.len(), name.span)
                    .and_then(|()| evaluator.expression(value, &BTreeMap::new()));
                if let Some(error) = evaluator.operation_error {
                    return Err(error);
                }
                shared_outputs = evaluator.take_shareable_outputs(evaluated.is_ok());
                total_steps = total_steps.saturating_add(evaluator.steps);
                retained_expansion = retained_expansion.saturating_add(evaluator.expansion);
                if total_steps > limits.max_total_steps {
                    return Err(EvaluationSetupError::EvaluationWorkLimit);
                }
                if retained_expansion > limits.max_retained_expansion_bytes {
                    return Err(EvaluationSetupError::EvaluationRetainedExpansionLimit);
                }
                let claims = std::mem::take(&mut evaluator.claims)
                    .into_iter()
                    .map(|(key, span)| ResourceClaim { key, span })
                    .collect();
                diagnostics_used = diagnostics_used.saturating_add(evaluator.diagnostics.len());
                let outcome = match evaluated {
                    Ok(value) if evaluator.diagnostics.is_empty() => {
                        RealizedRootOutcome::Value(value)
                    }
                    _ => RealizedRootOutcome::Failed(evaluator.diagnostics),
                };
                roots.push(RealizedRoot {
                    name: name.text.clone(),
                    identity: root.clone(),
                    ty,
                    outcome,
                    claims,
                });
            }
        }
    }
    Ok(RealizedProgram { roots })
}

fn validate_limits(limits: EvaluationLimits) -> Result<(), EvaluationSetupError> {
    let limits_and_maxima = [
        (
            EvaluationLimit::SetupSteps,
            limits.max_setup_steps,
            MAX_EVALUATION_SETUP_STEPS,
        ),
        (
            EvaluationLimit::SetupBytes,
            limits.max_setup_bytes,
            MAX_EVALUATION_SETUP_BYTES,
        ),
        (
            EvaluationLimit::TotalSteps,
            limits.max_total_steps,
            MAX_EVALUATION_TOTAL_STEPS,
        ),
        (
            EvaluationLimit::StepsPerRoot,
            limits.max_steps_per_root,
            MAX_EVALUATION_STEPS,
        ),
        (
            EvaluationLimit::Depth,
            limits.max_depth,
            MAX_EVALUATION_DEPTH,
        ),
        (
            EvaluationLimit::ExpansionBytes,
            limits.max_expansion_bytes,
            MAX_EVALUATION_EXPANSION,
        ),
        (
            EvaluationLimit::RetainedExpansionBytes,
            limits.max_retained_expansion_bytes,
            MAX_EVALUATION_RETAINED_EXPANSION,
        ),
        (
            EvaluationLimit::Diagnostics,
            limits.max_diagnostics,
            crate::MAX_DIAGNOSTICS,
        ),
    ];
    for (limit, configured, maximum) in limits_and_maxima {
        if configured > maximum {
            return Err(EvaluationSetupError::LimitExceedsMaximum {
                limit,
                configured,
                maximum,
            });
        }
    }
    Ok(())
}

type SpanKey = (usize, u32, u32);

fn span_key(span: Span) -> SpanKey {
    (span.source_id().index(), span.start(), span.end())
}
