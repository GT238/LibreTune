//! The per-turn agent orchestrator.
//!
//! Pipeline for one user turn:
//!   1. Gather context (caller-supplied snapshot of tune + realtime state).
//!   2. Build a [`ChatRequest`] with the system prompt, history, and the
//!      [`crate::agent::tools::catalogue`].
//!   3. Call the [`LlmClient`].
//!   4. Map the model's [`ToolCall`]s into [`Action`]s.
//!   5. Validate via [`ActionPlayer::validate_action_set`].
//!   6. Clamp table edits to [`AutoTuneAuthorityLimits`].
//!   7. Return a [`Proposal`] for the UI review queue.
//!
//! Nothing here applies anything. The orchestrator only *produces* a proposal;
//! application is a separate, user-triggered step.

use crate::action_scripting::{Action, ActionMetadata, ActionPlayer, ActionSet};
use crate::agent::safety::clamp_table_edit;
use crate::agent::tiers::{constant_safety_tier, ConstantSafetyTier};
use crate::agent::tools;
use crate::autotune::AutoTuneAuthorityLimits;
use crate::llm::types::{ChatRequest, FinishReason, LlmError, Message, ToolCall};
use crate::llm::LlmClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One proposed change, ready for the review queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedAction {
    /// The underlying action to apply if approved.
    pub action: Action,
    /// Safety tier (only meaningful for constant changes).
    pub safety_tier: ConstantSafetyTier,
    /// Validation outcome: warnings if it passed, errors if it failed.
    pub validation: ValidationResult,
    /// If clamped to authority limits, the original requested value.
    pub clamped_from: Option<f64>,
    /// Why the clamp happened (if it did).
    pub clamp_reason: Option<String>,
    /// Free-text reason the model gave (from the tool call's `reason` arg).
    pub reason: Option<String>,
}

/// Validation outcome for one proposed action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ValidationResult {
    /// Passed with non-fatal warnings (possibly empty).
    Ok { warnings: Vec<String> },
    /// Failed validation — must not be applied. Surfaced for the user to see
    /// what the model got wrong.
    Failed { errors: Vec<String> },
}

/// A complete proposal for one turn: the assistant's text reply plus the
/// proposed actions (some of which may have failed validation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    /// The model's natural-language reply (its explanation to the user).
    pub reply: String,
    /// Why the turn ended (tool_calls / stop / length / ...).
    pub finish_reason: String,
    /// Proposed actions, in the order the model emitted them.
    pub proposed: Vec<ProposedAction>,
    /// Whether every proposed action passed validation.
    pub all_valid: bool,
    /// Model-reported token usage (when available).
    pub usage: Option<crate::llm::types::Usage>,
}

/// Inputs the orchestrator needs that it cannot fetch itself (kept
/// provider-agnostic and I/O-free so the loop is unit-testable).
#[derive(Debug, Clone, Default)]
pub struct OrchestratorInputs {
    /// Conversation history *before* this turn's user message.
    pub history: Vec<Message>,
    /// The user's message this turn.
    pub user_message: String,
    /// A pre-rendered system prompt describing the tune/ECU context. Built by
    /// [`crate::agent::context`] from the live ECU state.
    pub system_prompt: String,
    /// Per-cell current values for tables the model might edit, keyed by
    /// table name → `(x,y)` → value. Used for authority clamping.
    pub current_table_values: HashMap<String, HashMap<(u16, u16), f64>>,
}

/// Run one agent turn. Does not mutate any ECU state.
pub async fn run_turn(
    client: &LlmClient,
    inputs: &OrchestratorInputs,
    authority: &AutoTuneAuthorityLimits,
) -> Result<Proposal, LlmError> {
    // 1. Assemble the request.
    let mut messages = Vec::with_capacity(inputs.history.len() + 2);
    messages.push(Message::system(&inputs.system_prompt));
    messages.extend(inputs.history.iter().cloned());
    messages.push(Message::user(&inputs.user_message));

    let req = ChatRequest::new(messages).with_tools(tools::catalogue());

    // 2. Call the provider.
    let resp = client.chat(&req).await?;

    // 3. Map tool calls → actions → validated/clamped proposal items.
    let mut proposed: Vec<ProposedAction> = Vec::with_capacity(resp.tool_calls.len());
    let mut all_valid = true;
    for tc in &resp.tool_calls {
        let mapped = map_tool_call(tc, inputs, authority);
        if matches!(mapped.validation, ValidationResult::Failed { .. }) {
            all_valid = false;
        }
        proposed.push(mapped);
    }

    Ok(Proposal {
        reply: resp.content.clone(),
        finish_reason: finish_reason_str(&resp.finish_reason),
        proposed,
        all_valid,
        usage: resp.usage,
    })
}

fn finish_reason_str(fr: &FinishReason) -> String {
    match fr {
        FinishReason::Stop => "stop".into(),
        FinishReason::ToolCalls => "tool_calls".into(),
        FinishReason::Length => "length".into(),
        FinishReason::ContentFilter => "content_filter".into(),
        FinishReason::Other(s) => s.clone(),
    }
}

/// Turn one model [`ToolCall`] into a [`ProposedAction`]. Propose-tools map
/// to an [`Action`]; read-tools are noted but not applied (the orchestrator
/// only proposes — reads are answered out-of-band by the command layer).
fn map_tool_call(
    tc: &ToolCall,
    inputs: &OrchestratorInputs,
    authority: &AutoTuneAuthorityLimits,
) -> ProposedAction {
    let args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
        Ok(v) => v,
        Err(e) => {
            return failed(
                Action::Pause { duration_ms: 0 },
                vec![format!("could not parse tool arguments: {e}")],
                tc,
            );
        }
    };

    match tc.name.as_str() {
        tools::tool_names::PROPOSE_TABLE_EDIT => {
            map_table_edit(&args, inputs, authority, tc).unwrap_or_else(|errs| {
                failed(
                    Action::Pause { duration_ms: 0 },
                    errs,
                    tc,
                )
            })
        }
        tools::tool_names::PROPOSE_CONSTANT_CHANGE => {
            map_constant_change(&args, tc).unwrap_or_else(|errs| {
                failed(Action::Pause { duration_ms: 0 }, errs, tc)
            })
        }
        tools::tool_names::PROPOSE_BULK_OP => {
            map_bulk_op(&args, tc).unwrap_or_else(|errs| failed(Action::Pause { duration_ms: 0 }, errs, tc))
        }
        // Read tools: they don't produce an action; surface as a no-op note.
        // The command layer answers read calls by feeding results back into
        // the next turn's history.
        _ => ProposedAction {
            action: Action::Pause { duration_ms: 0 },
            safety_tier: ConstantSafetyTier::Safe,
            validation: ValidationResult::Ok {
                warnings: vec![format!("read tool '{}' answered out-of-band", tc.name)],
            },
            clamped_from: None,
            clamp_reason: None,
            reason: Some(format!("read: {}", tc.name)),
        },
    }
}

fn map_table_edit(
    args: &serde_json::Value,
    inputs: &OrchestratorInputs,
    authority: &AutoTuneAuthorityLimits,
    _tc: &ToolCall,
) -> Result<ProposedAction, Vec<String>> {
    let table_name = get_str(args, "table_name")?;
    let x_index = get_u16(args, "x_index")?;
    let y_index = get_u16(args, "y_index")?;
    let new_value = get_f64(args, "new_value")?;
    let reason = get_str(args, "reason").ok();

    // Clamp to authority limits (needs current value).
    let current = inputs
        .current_table_values
        .get(&table_name)
        .and_then(|m| m.get(&(x_index, y_index)).copied());
    let action = Action::TableEdit {
        table_name: table_name.clone(),
        x_index,
        y_index,
        new_value,
        old_value: current,
    };
    let clamped = clamp_table_edit(action, authority, current);

    // Validate the (possibly clamped) action.
    let set = single_action_set(clamped.action.clone());
    let validation = match ActionPlayer::validate_action_set(&set, None) {
        Ok(w) => ValidationResult::Ok { warnings: w },
        Err(e) => ValidationResult::Failed { errors: e },
    };

    Ok(ProposedAction {
        action: clamped.action,
        safety_tier: ConstantSafetyTier::Caution,
        validation,
        clamped_from: clamped.clamped_from,
        clamp_reason: clamped.reason,
        reason,
    })
}

fn map_constant_change(args: &serde_json::Value, _tc: &ToolCall) -> Result<ProposedAction, Vec<String>> {
    let name = get_str(args, "name")?;
    let value = get_f64(args, "value")?;
    let reason = get_str(args, "reason").ok();
    let tier = constant_safety_tier(&name);

    let action = Action::ConstantChange {
        constant_name: name,
        new_value: value,
        old_value: None,
    };
    let set = single_action_set(action.clone());
    let validation = match ActionPlayer::validate_action_set(&set, None) {
        Ok(w) => ValidationResult::Ok { warnings: w },
        Err(e) => ValidationResult::Failed { errors: e },
    };

    Ok(ProposedAction {
        action,
        safety_tier: tier,
        validation,
        clamped_from: None,
        clamp_reason: None,
        reason,
    })
}

fn map_bulk_op(args: &serde_json::Value, _tc: &ToolCall) -> Result<ProposedAction, Vec<String>> {
    let table_name = get_str(args, "table_name")?;
    let operation = get_str(args, "operation")?;
    let reason = get_str(args, "reason").ok();

    let cells_arr = args
        .get("cells")
        .and_then(|v| v.as_array())
        .ok_or_else(|| vec!["missing 'cells' array".to_string()])?;
    let mut cells: Vec<(u16, u16)> = Vec::with_capacity(cells_arr.len());
    for c in cells_arr {
        let arr = c.as_array().ok_or_else(|| vec!["cell must be [x,y]".to_string()])?;
        if arr.len() < 2 {
            return Err(vec!["cell must have two elements".to_string()]);
        }
        let x = arr[0].as_u64().ok_or_else(|| vec!["cell x not integer".to_string()])? as u16;
        let y = arr[1].as_u64().ok_or_else(|| vec!["cell y not integer".to_string()])? as u16;
        cells.push((x, y));
    }

    let parameters: HashMap<String, f64> = args
        .get("parameters")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect()
        })
        .unwrap_or_default();

    let action = Action::BulkOperation {
        operation,
        table_name,
        cells,
        parameters,
        old_values: None,
    };
    let set = single_action_set(action.clone());
    let validation = match ActionPlayer::validate_action_set(&set, None) {
        Ok(w) => ValidationResult::Ok { warnings: w },
        Err(e) => ValidationResult::Failed { errors: e },
    };

    Ok(ProposedAction {
        action,
        safety_tier: ConstantSafetyTier::Caution,
        validation,
        clamped_from: None,
        clamp_reason: None,
        reason,
    })
}

fn failed(action: Action, errors: Vec<String>, tc: &ToolCall) -> ProposedAction {
    ProposedAction {
        action,
        safety_tier: ConstantSafetyTier::Caution,
        validation: ValidationResult::Failed { errors },
        clamped_from: None,
        clamp_reason: None,
        reason: Some(format!("tool '{}'", tc.name)),
    }
}

// --- small helpers -------------------------------------------------------

fn single_action_set(action: Action) -> ActionSet {
    ActionSet {
        id: "proposal".into(),
        name: "AI proposal".into(),
        description: "Single-action proposal from the assistant".into(),
        version: "1".into(),
        actions: vec![action],
        metadata: ActionMetadata {
            created_by: "ai-assistant".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            modified_at: chrono::Utc::now().to_rfc3339(),
            tags: vec!["ai-proposal".into()],
            compatible_ecus: vec![],
        },
    }
}

fn get_str(v: &serde_json::Value, key: &str) -> Result<String, Vec<String>> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| vec![format!("missing or non-string '{key}'")])
}

fn get_f64(v: &serde_json::Value, key: &str) -> Result<f64, Vec<String>> {
    v.get(key)
        .and_then(|x| x.as_f64())
        .ok_or_else(|| vec![format!("missing or non-numeric '{key}'")])
}

fn get_u16(v: &serde_json::Value, key: &str) -> Result<u16, Vec<String>> {
    v.get(key)
        .and_then(|x| x.as_u64())
        .map(|n| n as u16)
        .ok_or_else(|| vec![format!("missing or non-integer '{key}'")])
}

// Extend ChatRequest with a fluent .with_tools helper (local to this module
// to avoid adding a public builder for now).
trait ChatRequestExt {
    fn with_tools(self, tools: Vec<crate::llm::types::ToolDef>) -> Self;
}
impl ChatRequestExt for ChatRequest {
    fn with_tools(mut self, tools: Vec<crate::llm::types::ToolDef>) -> Self {
        self.tools = tools;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> AutoTuneAuthorityLimits {
        AutoTuneAuthorityLimits {
            max_cell_value_change: 5.0,
            max_cell_percentage_change: 10.0,
        }
    }

    #[test]
    fn maps_table_edit_and_clamps() {
        let tc = ToolCall {
            id: "1".into(),
            name: tools::tool_names::PROPOSE_TABLE_EDIT.into(),
            arguments: r#"{"table_name":"veTable1","x_index":0,"y_index":0,"new_value":60.0}"#.into(),
        };
        let mut inputs = OrchestratorInputs::default();
        inputs
            .current_table_values
            .entry("veTable1".into())
            .or_default()
            .insert((0, 0), 50.0);
        let pa = map_tool_call(&tc, &inputs, &auth());
        match pa.action {
            Action::TableEdit { new_value, .. } => {
                // 50 -> 60 clamped to 55 (per-cell limit 5).
                assert!((new_value - 55.0).abs() < 1e-9);
            }
            _ => panic!("expected TableEdit"),
        }
        assert!(pa.clamped_from.is_some());
    }

    #[test]
    fn maps_constant_change_with_tier() {
        let tc = ToolCall {
            id: "1".into(),
            name: tools::tool_names::PROPOSE_CONSTANT_CHANGE.into(),
            arguments: r#"{"name":"fanOutputPin","value":7}"#.into(),
        };
        let pa = map_tool_call(&tc, &OrchestratorInputs::default(), &auth());
        assert_eq!(pa.safety_tier, ConstantSafetyTier::Dangerous);
        match pa.validation {
            ValidationResult::Ok { .. } => {}
            ValidationResult::Failed { errors } => panic!("should pass: {errors:?}"),
        }
    }

    #[test]
    fn invalid_args_surface_as_failed() {
        let tc = ToolCall {
            id: "1".into(),
            name: tools::tool_names::PROPOSE_TABLE_EDIT.into(),
            arguments: r#"{"table_name":"veTable1"}"#.into(), // missing x_index etc
        };
        let pa = map_tool_call(&tc, &OrchestratorInputs::default(), &auth());
        assert!(matches!(pa.validation, ValidationResult::Failed { .. }));
    }
}
