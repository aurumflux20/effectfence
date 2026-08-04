//! MCP (Model Context Protocol) stdio server exposing the causal effect
//! fence (see [`core_rs::fence`]) as two tools -- `fence_prepare` and
//! `fence_commit` -- so agents can route side-effecting tool calls
//! through the fence instead of racing each other directly.
//!
//! Built on the official `rmcp` SDK rather than a hand-rolled JSON-RPC
//! layer.

use anyhow::Result;
use core_rs::fence::{
    commit_effect_cert, prepare_effect_fence, DomainFence, PreparedEffect, ReadSetEntry,
    VectorClock,
};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::Value;

/// Parameters for the `fence_prepare` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FencePrepareArgs {
    /// Hex SHA-256 hash of the effect certificate this one causally
    /// follows. Omit for a genesis effect with no prior cause.
    #[serde(default)]
    parent: Option<String>,
    /// The contention scope this effect will act on, e.g. `"order:123"`.
    domain: String,
    /// Name of the tool/effect being fenced, e.g. `"charge_card"`.
    tool: String,
    /// Arbitrary JSON arguments for the tool call being fenced.
    #[serde(default)]
    args: Value,
    /// Other domains this decision cross-checked, and the sequence
    /// observed for each. Do not include `domain` itself here -- see
    /// the `prepare_effect_fence` docs in `core_rs::fence` for why.
    #[serde(default)]
    read_set: Vec<ReadSetEntry>,
    /// Identifier of the calling agent.
    agent: String,
    /// The calling agent's current view of causal history.
    #[serde(default)]
    known_clock: VectorClock,
}

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

/// The MCP server. Holds the one [`DomainFence`] shared by every
/// `fence_prepare`/`fence_commit` call for the life of the process.
#[derive(Clone, Default)]
struct EffectFenceServer {
    fence: DomainFence,
}

#[tool_router]
impl EffectFenceServer {
    #[tool(
        description = "Validate an effect's read-set against current domain state and, if it still holds, atomically reserve the next sequence number for `domain`. Call this before running a side-effecting tool call. On success, the result is a PreparedEffect ticket (JSON) -- hold onto it and pass it back to fence_commit once the tool call finishes. On a DomainRace or ReadSetStale error, do NOT run the tool; another attempt already won, or a dependency changed."
    )]
    async fn fence_prepare(
        &self,
        Parameters(args): Parameters<FencePrepareArgs>,
    ) -> Result<CallToolResult, McpError> {
        match prepare_effect_fence(
            &self.fence,
            args.parent,
            args.domain,
            args.tool,
            args.args,
            args.read_set,
            args.agent,
            &args.known_clock,
        ) {
            Ok(prepared) => {
                let json = serde_json::to_string(&prepared)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
            }
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(err.to_string())])),
        }
    }

    #[tool(
        description = "Finalize a PreparedEffect ticket (from fence_prepare) into a content-addressed EffectCert once the tool call's result is known. Re-validates the read set one more time and rejects with ReadSetStale if a dependency moved while the effect was running."
    )]
    async fn fence_commit(
        &self,
        Parameters(args): Parameters<FenceCommitArgs>,
    ) -> Result<CallToolResult, McpError> {
        match commit_effect_cert(&self.fence, args.prepared, args.result) {
            Ok(cert) => {
                let json = serde_json::to_string(&cert)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
            }
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(err.to_string())])),
        }
    }
}

#[tool_handler(
    name = "effectfence",
    version = "0.1.0",
    instructions = "Causal effect fencing for multi-agent tool calls. Call fence_prepare before a side-effecting tool call and fence_commit after it finishes, so concurrent agents racing on the same domain can't both execute it."
)]
impl ServerHandler for EffectFenceServer {}

#[tokio::main]
async fn main() -> Result<()> {
    let service = EffectFenceServer::default().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
