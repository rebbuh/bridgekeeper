use crate::{
    constraint::{ConstraintInfo, ConstraintStoreRef},
    crd::Constraint,
    events::{ConstraintEvent, ConstraintEventData, EventSender},
};
use kube::core::{
    admission::{self, Operation},
    DynamicObject,
};
use lazy_static::lazy_static;
use prometheus::{register_counter_vec, CounterVec};
use pyo3::prelude::*;
use serde_derive::Serialize;
use std::sync::{Arc, Mutex};

lazy_static! {
    static ref MATCHED_CONSTRAINTS: CounterVec = register_counter_vec!(
        "bridgekeeper_constraint_matched",
        "Number of admissions matched to a constraint.",
        &["name"]
    )
    .unwrap();
    static ref CONSTRAINT_EVALUATIONS_SUCCESS: CounterVec = register_counter_vec!(
        "bridgekeeper_constraint_evaluated_success",
        "Number of successfull constraint evaluations",
        &["name"]
    )
    .unwrap();
    static ref CONSTRAINT_EVALUATIONS_REJECT: CounterVec = register_counter_vec!(
        "bridgekeeper_constraint_evaluated_reject",
        "Number of failed constraint evaluations.",
        &["name"]
    )
    .unwrap();
    static ref CONSTRAINT_EVALUATIONS_ERROR: CounterVec = register_counter_vec!(
        "bridgekeeper_constraint_evaluated_error",
        "Number of constraint evaluations that had an error.",
        &["name"]
    )
    .unwrap();
    static ref CONSTRAINT_VALIDATIONS_FAIL: CounterVec = register_counter_vec!(
        "bridgekeeper_constraint_validation_fail",
        "Number of constraint validations that failed.",
        &["name"]
    )
    .unwrap();
}

#[derive(Serialize)]
pub struct ValidationRequest {
    pub object: DynamicObject,
    pub operation: Operation,
}

impl ValidationRequest {
    pub fn from(
        admission_request: admission::AdmissionRequest<DynamicObject>,
    ) -> Option<ValidationRequest> {
        if let Some(object) = admission_request.object {
            Some(ValidationRequest {
                object,
                operation: admission_request.operation,
            })
        } else {
            None
        }
    }
}

pub struct ConstraintEvaluator {
    constraints: ConstraintStoreRef,
    event_sender: EventSender,
}

pub struct EvaluationResult {
    pub allowed: bool,
    pub reason: Option<String>,
    pub warnings: Vec<String>,
    pub patch: Option<json_patch::Patch>,
}

pub type ConstraintEvaluatorRef = Arc<Mutex<ConstraintEvaluator>>;

impl ConstraintEvaluator {
    pub fn new(
        constraints: ConstraintStoreRef,
        event_sender: EventSender,
    ) -> ConstraintEvaluatorRef {
        let evaluator = ConstraintEvaluator {
            constraints,
            event_sender,
        };
        pyo3::prepare_freethreaded_python();
        Arc::new(Mutex::new(evaluator))
    }

    pub fn evaluate_constraints(
        &self,
        admission_request: admission::AdmissionRequest<DynamicObject>,
    ) -> EvaluationResult {
        let mut warnings = Vec::new();
        let namespace = admission_request.namespace.clone();
        let name = admission_request.name.clone();
        let gvk = admission_request.kind.clone();
        let request = match ValidationRequest::from(admission_request) {
            Some(request) => request,
            None => {
                return EvaluationResult {
                    allowed: true,
                    reason: Some("no object in request".to_string()),
                    warnings: vec![],
                    patch: None,
                }
            }
        };

        if let Ok(constraints) = self.constraints.lock() {
            let mut patches: Option<json_patch::Patch> = None;
            for value in constraints.constraints.values() {
                if value.is_match(&gvk, &namespace) {
                    MATCHED_CONSTRAINTS
                        .with_label_values(&[value.name.as_str()])
                        .inc();
                    log::info!(
                        "Object {}.{}/{}/{} matches constraint {}",
                        gvk.kind,
                        gvk.group,
                        namespace.clone().unwrap_or_else(|| "-".to_string()),
                        name,
                        value.name
                    );
                    let target_identifier = format!(
                        "{}/{}/{}/{}",
                        gvk.group,
                        gvk.kind,
                        namespace.clone().unwrap_or_else(|| "-".to_string()),
                        name
                    );
                    let res = evaluate_constraint(value, &request);
                    if let Some(mut patch) = res.2 {
                        if let Some(patches) = patches.as_mut() {
                            patches.0.append(&mut patch.0);
                        } else {
                            patches = Some(patch);
                        }
                    }
                    self.event_sender
                        .send(ConstraintEvent {
                            constraint_reference: value.ref_info.clone(),
                            event_data: ConstraintEventData::Evaluated {
                                target_identifier,
                                result: res.0,
                                reason: res.1.clone(),
                            },
                        })
                        .unwrap_or_else(|err| log::warn!("Could not send event: {:?}", err));
                    if res.0 {
                        CONSTRAINT_EVALUATIONS_SUCCESS
                            .with_label_values(&[value.name.as_str()])
                            .inc();
                        log::info!("Constraint '{}' evaluates to {}", value.name, res.0);
                        if res.1.is_some() {
                            warnings.push(res.1.unwrap());
                        }
                    } else {
                        CONSTRAINT_EVALUATIONS_REJECT
                            .with_label_values(&[value.name.as_str()])
                            .inc();
                        log::info!(
                            "Constraint '{}' evaluates to {} with message '{}'",
                            value.name,
                            res.0,
                            res.1.as_ref().unwrap()
                        );
                        if value.constraint.enforce.unwrap_or(true) {
                            // If one constraint fails no need to evaluate the others
                            return EvaluationResult {
                                allowed: res.0,
                                reason: res.1,
                                warnings,
                                patch: None,
                            };
                        } else {
                            warnings.push(res.1.unwrap());
                        }
                    }
                }
            }
            EvaluationResult {
                allowed: true,
                reason: None,
                warnings,
                patch: patches,
            }
        } else {
            panic!("Could not lock constraints mutex");
        }
    }

    pub fn validate_constraint(
        &self,
        request: &admission::AdmissionRequest<Constraint>,
    ) -> (bool, Option<String>) {
        if let Some(constraint) = request.object.as_ref() {
            let python_code = constraint.spec.rule.python.clone();
            Python::with_gil(|py| {
                if let Err(err) = PyModule::from_code(py, &python_code, "rule.py", "bridgekeeper") {
                    CONSTRAINT_VALIDATIONS_FAIL
                        .with_label_values(&[constraint.metadata.name.as_ref().unwrap().as_str()])
                        .inc();
                    (false, Some(format!("Python compile error: {:?}", err)))
                } else {
                    (true, None)
                }
            })
        } else {
            (false, Some("No rule found".to_string()))
        }
    }
}

fn evaluate_constraint(
    constraint: &ConstraintInfo,
    request: &ValidationRequest,
) -> (bool, Option<String>, Option<json_patch::Patch>) {
    let name = &constraint.name;
    Python::with_gil(|py| {
        let obj = pythonize::pythonize(py, &request).unwrap();
        if let Ok(rule_code) = PyModule::from_code(
            py,
            &constraint.constraint.rule.python,
            "rule.py",
            "bridgekeeper",
        ) {
            if let Ok(validation_function) = rule_code.getattr("validate") {
                match validation_function.call1((obj,)) {
                    Ok(result) => extract_result(name, request, result),
                    Err(err) => fail(name, &format!("Validation function failed: {}", err)),
                }
            } else {
                fail(name, "Validation function not found in code")
            }
        } else {
            fail(name, "Validation function could not be compiled")
        }
    })
}

pub fn evaluate_constraint_audit(
    constraint: &ConstraintInfo,
    object: DynamicObject,
) -> (bool, Option<String>, Option<json_patch::Patch>) {
    let request = ValidationRequest {
        object,
        operation: Operation::Update,
    };
    evaluate_constraint(constraint, &request)
}

fn extract_result(
    name: &String,
    request: &ValidationRequest,
    result: &PyAny,
) -> (bool, Option<String>, Option<json_patch::Patch>) {
    if let Ok((code, reason, patched)) = result.extract::<(bool, Option<String>, &PyAny)>() {
        if let Ok(result) = pythonize::depythonize::<serde_json::Value>(patched) {
            match generate_patches(&request.object, &result) {
                Ok(patch) => (code, reason, Some(patch)),
                Err(error) => fail(name, &format!("failed to compute patch: {}", error)),
            }
        } else {
            fail(
                name,
                "Could not read patched object returned by validation function",
            )
        }
    } else if let Ok((code, reason)) = result.extract::<(bool, Option<String>)>() {
        (code, reason, None)
    } else if let Ok(code) = result.extract::<bool>() {
        (code, None, None)
    } else {
        fail(name, "Validation function did not return expected types")
    }
}

fn fail(name: &str, reason: &str) -> (bool, Option<String>, Option<json_patch::Patch>) {
    CONSTRAINT_EVALUATIONS_ERROR
        .with_label_values(&[name])
        .inc();
    (false, Some(reason.to_string()), None)
}

fn generate_patches(
    input: &DynamicObject,
    patched: &serde_json::Value,
) -> Result<json_patch::Patch, String> {
    let input = match serde_json::to_value(input) {
        Ok(input) => input,
        Err(error) => return Err(error.to_string()),
    };
    Ok(json_patch::diff(&input, patched))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::ConstraintSpec;
    use kube::core::ObjectMeta;

    #[test]
    fn test_simple_evaluate() {
        pyo3::prepare_freethreaded_python();
        let python = r#"
def validate(request):
    return True
        "#;
        let constraint_spec = ConstraintSpec::from_python(python.to_string());
        let constraint =
            ConstraintInfo::new("test".to_string(), constraint_spec, Default::default());

        let object = DynamicObject {
            types: None,
            metadata: ObjectMeta::default(),
            data: serde_json::Value::Null,
        };
        let request = ValidationRequest {
            object,
            operation: Operation::Create,
        };

        let (res, reason, patch) = evaluate_constraint(&constraint, &request);
        assert!(res, "validate function failed: {}", reason.unwrap());
        assert!(reason.is_none());
        assert!(patch.is_none());
    }

    #[test]
    fn test_simple_evaluate_with_reason() {
        pyo3::prepare_freethreaded_python();
        let python = r#"
def validate(request):
    return False, "foobar"
        "#;
        let constraint_spec = ConstraintSpec::from_python(python.to_string());
        let constraint =
            ConstraintInfo::new("test".to_string(), constraint_spec, Default::default());

        let object = DynamicObject {
            types: None,
            metadata: ObjectMeta::default(),
            data: serde_json::Value::Null,
        };
        let request = ValidationRequest {
            object,
            operation: Operation::Create,
        };

        let (res, reason, patch) = evaluate_constraint(&constraint, &request);
        assert!(!res);
        assert!(reason.is_some());
        assert_eq!("foobar".to_string(), reason.unwrap());
        assert!(patch.is_none());
    }

    #[test]
    fn test_evaluate_with_invalid_python() {
        pyo3::prepare_freethreaded_python();
        let python = r#"
def validate(request):
    return false, "foobar"
        "#;
        let constraint_spec = ConstraintSpec::from_python(python.to_string());
        let constraint =
            ConstraintInfo::new("test".to_string(), constraint_spec, Default::default());

        let object = DynamicObject {
            types: None,
            metadata: ObjectMeta::default(),
            data: serde_json::Value::Null,
        };
        let request = ValidationRequest {
            object,
            operation: Operation::Create,
        };

        let (res, reason, patch) = evaluate_constraint(&constraint, &request);
        assert!(!res);
        assert!(reason.is_some());
        assert_eq!(
            "Validation function failed: NameError: name 'false' is not defined".to_string(),
            reason.unwrap()
        );
        assert!(patch.is_none());
    }

    #[test]
    fn test_simple_mutate() {
        pyo3::prepare_freethreaded_python();
        let python = r#"
def validate(request):
    object = request["object"]
    object["b"] = "2"
    return True, None, object
        "#;
        let constraint_spec = ConstraintSpec::from_python(python.to_string());
        let constraint =
            ConstraintInfo::new("test".to_string(), constraint_spec, Default::default());

        let data = serde_json::from_str(r#"{"a": 1, "b": "1"}"#).unwrap();
        let object = DynamicObject {
            types: None,
            metadata: ObjectMeta::default(),
            data,
        };
        let request = ValidationRequest {
            object,
            operation: Operation::Create,
        };

        let (res, reason, patch) = evaluate_constraint(&constraint, &request);
        assert!(res, "validate function failed: {}", reason.unwrap());
        assert!(reason.is_none());
        assert!(patch.is_some());
        let patch = patch.unwrap();
        assert_eq!(1, patch.0.len());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                r#"[{"op": "replace", "path": "/b", "value": "2"}]"#
            )
            .unwrap(),
            serde_json::to_value(patch.0).unwrap()
        );
    }
}
