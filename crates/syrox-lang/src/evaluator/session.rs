use std::{collections::BTreeMap, fmt, mem::size_of, sync::Arc};

use crate::{
    CanonicalItemIdentity, CheckPolicy, CheckedProgram, Expression, Ident, ItemId, ItemKind,
    OutputKind, SourceDomainId, Ty,
};

use super::{
    EvaluationEnvironment, EvaluationLimits, EvaluationSetupError, RealizedProgram, RealizedRoot,
    RealizedRootOutcome, ResourceClaim, Value,
    index::{ProgramIndex, SetupBudget},
    runtime::Evaluator,
    validate_limits,
};

#[cfg(test)]
mod tests;

struct Root<'a> {
    name: &'a Ident,
    expression: &'a Expression,
    item: ItemId,
    identity: Arc<CanonicalItemIdentity>,
}

/// One pure evaluation operation over an already checked snapshot. Queries
/// share budgets and claim-free output values, and do not execute other roots.
/// Returned roots are borrowed: querying an affine result does not duplicate it.
pub struct EvaluationSession<'a> {
    program: &'a CheckedProgram,
    policy: &'a CheckPolicy,
    environment: &'a EvaluationEnvironment,
    limits: EvaluationLimits,
    index: ProgramIndex<'a>,
    roots: Vec<Root<'a>>,
    names: BTreeMap<&'a str, usize>,
    results: Vec<Option<RealizedRoot>>,
    shared_outputs: BTreeMap<ItemId, Value>,
    total_steps: usize,
    retained_expansion: usize,
    diagnostics_used: usize,
    failure: Option<EvaluationSetupError>,
}

impl fmt::Debug for EvaluationSession<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvaluationSession")
            .field("roots", &self.names.keys())
            .field("total_steps", &self.total_steps)
            .field("retained_expansion", &self.retained_expansion)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl<'a> EvaluationSession<'a> {
    pub fn new(
        program: &'a CheckedProgram,
        policy: &'a CheckPolicy,
        environment: &'a EvaluationEnvironment,
        limits: EvaluationLimits,
    ) -> Result<Self, EvaluationSetupError> {
        validate_limits(limits)?;
        program.require_policy(policy).map_err(|mismatch| {
            EvaluationSetupError::PolicyMismatch {
                checked: mismatch.checked,
                supplied: mismatch.supplied,
            }
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
        let mut budget = SetupBudget::new(limits);
        let index = ProgramIndex::new(program, policy, &mut budget)?;
        let mut roots = Vec::new();
        let mut names = BTreeMap::new();
        for source in program.resolved().parsed().iter() {
            budget.charge(0)?;
            if source.domain() != SourceDomainId::project() {
                continue;
            }
            for item in &source.program().items {
                budget.charge(0)?;
                let ItemKind::Outputs(outputs) = &item.kind else {
                    continue;
                };
                for output in &outputs.entries {
                    budget.charge(0)?;
                    let OutputKind::Value { name, value, .. } = &output.kind else {
                        continue;
                    };
                    budget.charge(
                        2 * size_of::<Root<'a>>()
                            + size_of::<(&str, usize)>()
                            + size_of::<Option<RealizedRoot>>(),
                    )?;
                    let identity = index
                        .identity_at(output.span)
                        .ok_or(EvaluationSetupError::MissingRootIdentity)?
                        .clone();
                    let item = index
                        .item_at(output.span)
                        .ok_or(EvaluationSetupError::MissingRootIdentity)?;
                    names.insert(name.text.as_str(), roots.len());
                    roots.push(Root {
                        name,
                        expression: value,
                        item,
                        identity,
                    });
                }
            }
        }
        let results = std::iter::repeat_with(|| None).take(roots.len()).collect();
        Ok(Self {
            program,
            policy,
            environment,
            limits,
            index,
            roots,
            names,
            results,
            shared_outputs: BTreeMap::new(),
            total_steps: 0,
            retained_expansion: 0,
            diagnostics_used: 0,
            failure: None,
        })
    }

    /// Deterministic enumeration of top-level value outputs without evaluation.
    pub fn root_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.names.keys().copied()
    }

    pub fn root_type(&self, name: &str) -> Option<&Ty> {
        let root = &self.roots[*self.names.get(name)?];
        self.program
            .expression(root.expression.span)
            .map(crate::CheckedExpression::ty)
    }

    pub fn evaluate_root(&mut self, name: &str) -> Result<&RealizedRoot, EvaluationQueryError> {
        if let Some(error) = self.failure {
            return Err(error.into());
        }
        let index = *self
            .names
            .get(name)
            .ok_or(EvaluationQueryError::UnknownRoot)?;
        self.evaluate_index(index)?;
        Ok(self.results[index]
            .as_ref()
            .expect("queried root has an outcome"))
    }

    pub fn evaluate_all(&mut self) -> Result<(), EvaluationSetupError> {
        for index in 0..self.roots.len() {
            self.evaluate_index(index)?;
        }
        Ok(())
    }

    /// Only requested roots enter the result, in source order. An exhausted
    /// operation cannot be converted into a partially successful program.
    pub fn into_realized(self) -> Result<RealizedProgram, EvaluationSetupError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        Ok(RealizedProgram {
            roots: self.results.into_iter().flatten().collect(),
        })
    }

    fn evaluate_index(&mut self, index: usize) -> Result<(), EvaluationSetupError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.results[index].is_some() {
            return Ok(());
        }
        let outcome = self.evaluate_uncached(index);
        if let Err(error) = outcome {
            self.failure = Some(error);
        }
        outcome
    }

    fn evaluate_uncached(&mut self, index: usize) -> Result<(), EvaluationSetupError> {
        let root = &self.roots[index];
        self.retained_expansion = self
            .retained_expansion
            .saturating_add(size_of::<RealizedRoot>());
        if self.retained_expansion > self.limits.max_retained_expansion_bytes {
            return Err(EvaluationSetupError::EvaluationRetainedExpansionLimit);
        }
        let limits = EvaluationLimits {
            max_total_steps: self.limits.max_total_steps.saturating_sub(self.total_steps),
            max_retained_expansion_bytes: self.limits.max_retained_expansion_bytes
                - self.retained_expansion,
            max_diagnostics: self
                .limits
                .max_diagnostics
                .saturating_sub(self.diagnostics_used),
            ..self.limits
        };
        let mut evaluator = Evaluator::new(
            self.program,
            self.policy,
            self.environment,
            &self.index,
            limits,
            root.expression.span,
            root.identity.clone(),
        );
        evaluator.inherit_outputs(std::mem::take(&mut self.shared_outputs));
        let ty = evaluator.canonical_expression_type(root.expression);
        if let Some(error) = evaluator.operation_error {
            return Err(error);
        }
        let evaluated = evaluator
            .expand(root.name.text.len(), root.name.span)
            .and_then(|()| evaluator.root_value(root.item, root.expression));
        if let Some(error) = evaluator.operation_error {
            return Err(error);
        }
        self.shared_outputs = evaluator.take_shareable_outputs(evaluated.is_ok());
        self.total_steps = self.total_steps.saturating_add(evaluator.steps);
        self.retained_expansion = self.retained_expansion.saturating_add(evaluator.expansion);
        if self.total_steps > self.limits.max_total_steps {
            return Err(EvaluationSetupError::EvaluationWorkLimit);
        }
        if self.retained_expansion > self.limits.max_retained_expansion_bytes {
            return Err(EvaluationSetupError::EvaluationRetainedExpansionLimit);
        }
        let claims = std::mem::take(&mut evaluator.claims)
            .into_iter()
            .map(|(key, span)| ResourceClaim { key, span })
            .collect();
        self.diagnostics_used = self
            .diagnostics_used
            .saturating_add(evaluator.diagnostics.len());
        let outcome = match evaluated {
            Ok(value) if evaluator.diagnostics.is_empty() => RealizedRootOutcome::Value(value),
            _ => RealizedRootOutcome::Failed(evaluator.diagnostics),
        };
        self.results[index] = Some(RealizedRoot {
            name: root.name.text.clone(),
            identity: root.identity.clone(),
            ty,
            outcome,
            claims,
        });
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvaluationQueryError {
    UnknownRoot,
    Setup(EvaluationSetupError),
}

impl From<EvaluationSetupError> for EvaluationQueryError {
    fn from(error: EvaluationSetupError) -> Self {
        Self::Setup(error)
    }
}

impl fmt::Display for EvaluationQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRoot => f.write_str("requested value output does not exist"),
            Self::Setup(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for EvaluationQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Setup(error) => Some(error),
            Self::UnknownRoot => None,
        }
    }
}
