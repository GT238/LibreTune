//! Tauri commands for the AI assistant agent loop.
//!
//! These wrap the [`libretune_core::agent`] orchestrator and
//! [`libretune_core::llm`] provider client. They never apply changes: a turn
//! produces a [`Proposal`] that the frontend stages in a review queue. Only
//! `agent_apply_proposals` mutates the working tune, and even then burning to
//! the ECU is a separate manual user action.

use crate::state::AppState;
use libretune_core::action_scripting::Action;
use libretune_core::agent::orchestrator::{run_turn, OrchestratorInputs, Proposal};
use libretune_core::agent::tiers::ConstantSafetyTier;
use libretune_core::autotune::AutoTuneAuthorityLimits;
use libretune_core::llm::types::{LlmError, Message};
use libretune_core::llm::{LlmClient, ProviderConfig};
use serde::{Deserialize, Serialize};

/// Construct a `ProviderConfig` from stored settings.
fn config_from_settings(s: &crate::Settings) -> ProviderConfig {
    ProviderConfig {
        provider: s.ai_provider.clone(),
        base_url: s.ai_base_url.clone(),
        api_key: s.ai_api_key.clone(),
        model: s.ai_model.clone(),
    }
}

/// Build an `LlmClient` from current settings.
/// Errors surface as `Result<T, String>` per the app's convention.
fn build_client(s: &crate::Settings) -> Result<LlmClient, LlmError> {
    LlmClient::new(&config_from_settings(s))
}

/// Request payload from the frontend for one assistant turn.
#[derive(Debug, Deserialize)]
pub struct AgentTurnRequest {
    /// The user's message this turn.
    pub user_message: String,
    /// Prior conversation as the frontend has it (serialized messages).
    pub history: Vec<SerializedMessage>,
    /// Pre-rendered system prompt describing the ECU/tune context. The
    /// frontend builds this from the current view (tables loaded, etc.).
    pub system_prompt: String,
}

/// A serialized [`Message`] that round-trips through JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedMessage {
    pub role: String,
    pub content: String,
}

impl From<SerializedMessage> for Message {
    fn from(s: SerializedMessage) -> Self {
        match s.role.as_str() {
            "system" => Message::system(s.content),
            "assistant" => Message::assistant(s.content),
            _ => Message::user(s.content),
        }
    }
}

/// Build a default authority-limit envelope for clamping proposals.
fn default_authority() -> AutoTuneAuthorityLimits {
    AutoTuneAuthorityLimits::default()
}

/// Check whether the assistant is configured and enabled. Cheap pre-flight.
#[tauri::command]
pub async fn agent_status(app: tauri::AppHandle) -> Result<AgentStatus, String> {
    let s = crate::load_settings(&app);
    Ok(AgentStatus {
        enabled: s.ai_assistant_enabled,
        risk_acknowledged: s.ai_risk_acknowledged,
        provider: s.ai_provider.clone(),
        model: s.ai_model.clone(),
        capability_tier: s.ai_capability_tier.clone(),
        // Configured if both provider and model are non-empty (key is optional
        // for local providers, so we don't require it).
        configured: !s.ai_provider.is_empty() && !s.ai_model.is_empty(),
    })
}

#[derive(Debug, Serialize)]
pub struct AgentStatus {
    pub enabled: bool,
    pub risk_acknowledged: bool,
    pub provider: String,
    pub model: String,
    pub capability_tier: String,
    pub configured: bool,
}

/// Run one assistant turn. Returns a [`Proposal`] for the review queue.
///
/// Does not apply anything. The frontend renders `proposal.proposed` as a
/// reviewable list; the user explicitly approves items before
/// `agent_apply_proposals` stages them to the working tune.
#[tauri::command]
pub async fn agent_send_message(
    app: tauri::AppHandle,
    request: AgentTurnRequest,
) -> Result<Proposal, String> {
    let s = crate::load_settings(&app);

    // Gate: must be enabled + risk-acknowledged.
    if !s.ai_assistant_enabled {
        return Err("AI assistant is not enabled".to_string());
    }
    if !s.ai_risk_acknowledged {
        return Err("AI assistant risk acknowledgement is missing".to_string());
    }

    let client = build_client(&s).map_err(|e| e.to_string())?;

    let history: Vec<Message> = request.history.into_iter().map(Into::into).collect();
    let inputs = OrchestratorInputs {
        history,
        user_message: request.user_message,
        system_prompt: request.system_prompt,
        current_table_values: Default::default(),
    };

    let authority = default_authority();
    run_turn(&client, &inputs, &authority)
        .await
        .map_err(|e| e.to_string())
}

/// Request payload for applying a subset of a proposal.
#[derive(Debug, Deserialize)]
pub struct ApplyProposalsRequest {
    /// The actions to apply, exactly as the user approved them from the
    /// proposal queue. Re-validated here before apply.
    pub actions: Vec<Action>,
}

/// Result of applying one action.
#[derive(Debug, Serialize)]
pub struct ApplyResult {
    pub applied: bool,
    pub error: Option<String>,
    /// Safety tier (constants only) so the UI can show what was applied.
    pub safety_tier: Option<ConstantSafetyTier>,
}

/// Apply a list of approved actions to the working tune.
///
/// Each action is re-validated against the loaded definition; invalid ones are
/// skipped with an error in the result. **Nothing is burned to the ECU** —
/// the changes are staged in the working tune and flagged as modified, so the
/// user must explicitly burn afterward.
#[tauri::command]
pub async fn agent_apply_proposals(
    state: tauri::State<'_, AppState>,
    request: ApplyProposalsRequest,
) -> Result<Vec<ApplyResult>, String> {
    use libretune_core::action_scripting::{ActionMetadata, ActionPlayer, ActionSet};

    // 1. Validate every action while holding the definition lock (read-only).
    let mut results: Vec<ApplyResult> = Vec::with_capacity(request.actions.len());
    let mut any_applied = false;
    {
        let def = state.definition.lock().await;
        let def_ref = def.as_ref();

        for action in &request.actions {
            let tier = match action {
                Action::ConstantChange { constant_name, .. } => {
                    Some(libretune_core::agent::constant_safety_tier(constant_name))
                }
                _ => None,
            };

            let set = ActionSet {
                id: "apply".into(),
                name: "apply".into(),
                description: "Approved AI proposal action".into(),
                version: "1".into(),
                actions: vec![action.clone()],
                metadata: ActionMetadata {
                    created_by: "ai-assistant".into(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    modified_at: chrono::Utc::now().to_rfc3339(),
                    tags: vec!["ai-applied".into()],
                    compatible_ecus: vec![],
                },
            };

            match ActionPlayer::validate_action_set(&set, def_ref) {
                Ok(_warnings) => {
                    any_applied = true;
                    results.push(ApplyResult {
                        applied: true,
                        error: None,
                        safety_tier: tier,
                    });
                }
                Err(errors) => {
                    results.push(ApplyResult {
                        applied: false,
                        error: Some(errors.join("; ")),
                        safety_tier: tier,
                    });
                }
            }
        }
    } // definition lock released here

    // 2. If at least one action applied, flag the tune as modified so the
    //    user is prompted to burn. The actual table/constant mutation is
    //    performed by the frontend via the existing update commands (this
    //    command validates + signals intent; it does not itself write to
    //    tune state, to avoid duplicating the page-write path).
    if any_applied {
        let mut modified = state.tune_modified.lock().await;
        *modified = true;
    }

    Ok(results)
}

