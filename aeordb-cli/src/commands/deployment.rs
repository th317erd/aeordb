use aeordb::engine::{EngineError, EngineResult};
use aeordb::engine::v4::deployment_guard::{
  DeploymentDecisionV1, DeploymentTransitionStateV1, TRANSITION_RECOVERY_CAPABILITY_V1, current_deployment_capabilities,
  acquire_deployment_inspection_lock, evaluate_deployment_candidate, inspect_deployment_transition_state_read_only,
};
use serde::Serialize;

#[derive(Serialize)]
struct DeploymentCheckOutput<'a> {
  state: &'a DeploymentTransitionStateV1,
  decision: &'a DeploymentDecisionV1,
}

pub fn print_capabilities(json: bool, required: Option<&str>) -> Result<bool, serde_json::Error> {
  let report = current_deployment_capabilities();
  let supported = required.is_none_or(|required| report.capabilities.iter().any(|capability| capability == required));
  if json {
    println!("{}", serde_json::to_string(&report)?);
  } else if let Some(required) = required {
    if supported {
      println!("supported: {required}");
    } else {
      eprintln!("unsupported deployment capability: {required}");
    }
  } else {
    println!("AeorDB deployment capability protocol {}", report.protocol_version);
    for capability in &report.capabilities {
      println!("  {capability}");
    }
  }
  Ok(supported)
}

pub fn check_database(database: &str, candidate_capability: Option<&str>, json: bool) -> EngineResult<DeploymentDecisionV1> {
  let candidate_capability = candidate_capability.filter(|value| *value == TRANSITION_RECOVERY_CAPABILITY_V1);
  let _inspection_lock = if candidate_capability.is_none() { Some(acquire_deployment_inspection_lock(database)?) } else { None };
  let state = inspect_deployment_transition_state_read_only(database)?;
  let decision = evaluate_deployment_candidate(&state, candidate_capability);
  if json {
    let output = DeploymentCheckOutput { state: &state, decision: &decision };
    let output = serde_json::to_string(&output)
      .map_err(|error| EngineError::JsonParseError(format!("deployment check output serialization failed: {error}")))?;
    println!("{output}");
  } else {
    println!("database: {database}");
    println!("transition recovery required: {}", state.requires_transition_capability);
    println!("external spill artifacts: {}", state.external_spill_count);
    println!("decision: {}", if decision.allowed { "allowed" } else { "refused" });
    println!("{}", decision.message);
  }
  Ok(decision)
}
