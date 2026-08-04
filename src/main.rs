//! MCP (Model Context Protocol) stdio server exposing the causal effect
//! fence (see [`effectfence::fence`]) as three tools -- `fence_prepare`,
//! `fence_commit`, and `fence_abort` -- so agents can route
//! side-effecting tool calls through the fence instead of racing each
//! other (or re-running each other's completed work) directly.
//!
//! Built on the official `rmcp` SDK rather than a hand-rolled JSON-RPC
//! layer.

use anyhow::Result;
use effectfence::fence::{
    Admission, EffectFence, EffectRequest, PreparedEffect, abort_effect, commit_effect_cert,
    prepare_effect_fence,
};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use serde_json::Value;

/// Parameters for the `fence_commit` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FenceCommitArgs {
    /// The prepared-effect ticket returned by a prior `fence_prepare`
    /// call, passed back verbatim.
    prepared: PreparedEffect,
    /// The actual result of running the tool/effect.
    #[serde(default)]
    result: Value,
}

/// Parameters for the `fence_abort` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FenceAbortArgs {
    /// The prepared-effect ticket returned by a prior `fence_prepare`
    /// call, passed back verbatim.
    prepared: PreparedEffect,
    /// Why the effect failed (or why its outcome is unknown).
    reason: String,
}

/// The MCP server. Holds the one [`EffectFence`] shared by every tool
/// call for the life of the process.
#[derive(Clone, Default)]
struct EffectFenceServer {
    fence: EffectFence,
}

fn json_result(value: Value) -> Result<CallToolResult, McpError> {
    let json =
        serde_json::to_string(&value).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
}

#[tool_router]
impl EffectFenceServer {
    #[tool(
        description = "Ask permission to run a side-effecting tool call. `intent` is a stable id for the logical action (e.g. `charge:order-123`) -- attempts sharing an intent are the SAME action and only one will ever execute. Returns {status:'fresh', prepared} when this attempt wins (run the tool, then call fence_commit or fence_abort with the ticket), or {status:'already_done', cert} when this exact action already ran (use the recorded result; do NOT run the tool). Errors mean do not run: another attempt is in flight, a previous attempt failed (reconcile first), a dependency changed (ReadSetStale), or a same-instant race was lost (DomainRace)."
    )]
    async fn fence_prepare(
        &self,
        Parameters(req): Parameters<EffectRequest>,
    ) -> Result<CallToolResult, McpError> {
        match prepare_effect_fence(&self.fence, req) {
            Ok(Admission::Fresh(prepared)) => {
                json_result(serde_json::json!({"status": "fresh", "prepared": prepared}))
            }
            Ok(Admission::Replay(cert)) => {
                json_result(serde_json::json!({"status": "already_done", "cert": &*cert}))
            }
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(
                err.to_string(),
            )])),
        }
    }

    #[tool(
        description = "Report that a prepared effect finished, and mint its content-addressed EffectCert. Re-validates the read set one more time; if a dependency moved while the effect was running, the commit is rejected AND the intent is fenced as failed (the effect did run -- reconcile before retrying). On success, later fence_prepare calls with the same intent replay this cert instead of re-running the action."
    )]
    async fn fence_commit(
        &self,
        Parameters(args): Parameters<FenceCommitArgs>,
    ) -> Result<CallToolResult, McpError> {
        match commit_effect_cert(&self.fence, args.prepared, args.result) {
            Ok(cert) => json_result(serde_json::json!({"status": "committed", "cert": cert})),
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(
                err.to_string(),
            )])),
        }
    }

    #[tool(
        description = "Report that a prepared effect failed or its outcome is unknown. The intent stays fenced -- later fence_prepare calls with the same intent are rejected instead of silently re-running an action whose side effect may have fired -- until an operator reconciles with the downstream system and clears it."
    )]
    async fn fence_abort(
        &self,
        Parameters(args): Parameters<FenceAbortArgs>,
    ) -> Result<CallToolResult, McpError> {
        let intent = args.prepared.intent.clone();
        abort_effect(&self.fence, args.prepared, args.reason);
        json_result(serde_json::json!({"status": "aborted", "intent": intent}))
    }
}

#[tool_handler(
    name = "effectfence",
    version = "0.1.1", // KEEP IN SYNC with Cargo.toml -- the macro rejects env!(); test below enforces it
    instructions = "Causal effect fencing for multi-agent tool calls. Before any side-effecting tool call (charging, sending, provisioning), call fence_prepare with a stable `intent` id for the action; run the tool only on {status:'fresh'}, then report the outcome with fence_commit (success) or fence_abort (failure). Duplicates of a completed action get its recorded cert back instead of running again, and concurrent attempts at the same action are serialized to exactly one winner."
)]
impl ServerHandler for EffectFenceServer {}

#[tokio::main]
async fn main() -> Result<()> {
    let service = EffectFenceServer::default().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod version_sync {
    /// The tool_handler macro only accepts a string literal for `version`,
    /// so it cannot use env!("CARGO_PKG_VERSION") directly. This test is
    /// the tripwire: bump Cargo.toml without bumping the literal above and
    /// the suite fails.
    #[test]
    fn server_version_literal_matches_cargo_toml() {
        let src = include_str!("main.rs");
        let needle = format!("version = \"{}\"", env!("CARGO_PKG_VERSION"));
        assert!(
            src.contains(&needle),
            "MCP serverInfo version literal is out of sync with Cargo.toml ({})",
            env!("CARGO_PKG_VERSION")
        );
    }
}
