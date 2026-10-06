//! Input validation against the plugin's stored JSON Schema contract.

use serde_json::Value;

use crate::error::{ApiError, ErrorKind};

/// Compile a contract. Called at upload time (rejecting invalid schemas) and
/// again cheaply per task creation.
pub fn compile_contract(schema: &Value) -> Result<jsonschema::Validator, ApiError> {
    jsonschema::validator_for(schema).map_err(|e| {
        ApiError::new(
            ErrorKind::InvalidContract,
            "contract is not a valid JSON Schema",
        )
        .with_details(serde_json::json!({ "schema_error": e.to_string() }))
    })
}

/// Validate a task input against the contract. Violations are reported with
/// per-path details and never reach the executor.
pub fn validate_input(validator: &jsonschema::Validator, input: &Value) -> Result<(), ApiError> {
    if validator.is_valid(input) {
        return Ok(());
    }
    let violations: Vec<String> = validator
        .iter_errors(input)
        .take(8)
        .map(|e| e.to_string())
        .collect();
    Err(ApiError::new(
        ErrorKind::ContractViolation,
        "input does not satisfy the plugin contract",
    )
    .with_details(serde_json::json!({ "violations": violations })))
}
