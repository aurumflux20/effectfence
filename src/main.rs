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
    // Any JSON type -- a bare `Value` emits no `type`, which MCP clients
    // cannot render. See effectfence::fence::any_json_schema.
    #[serde(default)]
    #[schemars(schema_with = "effectfence::fence::any_json_schema")]
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

    #[tool(
        description = "Live counters since this fence process started: how many effects were admitted (ran), replayed (duplicate handed a recorded result), and refused (stale read-set, lost domain race, already in flight, or fenced after an unknown-outcome failure). `prevented` is every attempt that did NOT run the effect. Takes no arguments."
    )]
    async fn fence_stats(&self) -> Result<CallToolResult, McpError> {
        let s = self.fence.stats();
        json_result(serde_json::json!({
            "admitted": s.admitted,
            "replayed": s.replayed,
            "refused": {
                "stale_read_set": s.refused_stale,
                "domain_race": s.refused_race,
                "in_flight": s.refused_inflight,
                "prior_failure": s.refused_failed,
            },
            "total_attempts": s.total(),
            "prevented": s.prevented(),
            "log_line": s.log_line(),
        }))
    }
}

#[tool_handler(
    name = "effectfence",
    version = "0.1.7", // KEEP IN SYNC with Cargo.toml -- the macro rejects env!(); test below enforces it
    instructions = "Causal effect fencing for multi-agent tool calls. Before any side-effecting tool call (charging, sending, provisioning), call fence_prepare with a stable `intent` id for the action; run the tool only on {status:'fresh'}, then report the outcome with fence_commit (success) or fence_abort (failure). Duplicates of a completed action get its recorded cert back instead of running again, and concurrent attempts at the same action are serialized to exactly one winner."
)]
impl ServerHandler for EffectFenceServer {}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "wrap" {
        // `effectfence wrap -- <command> [args...]` (the `--` is optional)
        let child: Vec<String> = args[2..]
            .iter()
            .skip_while(|a| a.as_str() == "--")
            .cloned()
            .collect();
        return effectfence::wrap::run_wrap(child).await;
    }
    if args.len() >= 2 && args[1] == "probe" {
        // `effectfence probe --tool <name> [--args '<json>'] [--calls N] -- <command> [args...]`
        return effectfence::probe::run_probe(args[2..].to_vec()).await;
    }
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

#[cfg(test)]
mod server_json_sync {
    /// `server.json` carries its own copy of the version for the MCP
    /// Registry, and nothing else checks it. It drifted from the package
    /// version once already, which publishes a listing pointing at a
    /// release that isn't the current one.
    #[test]
    fn server_json_version_matches_cargo_toml() {
        let raw = include_str!("../server.json");
        let doc: serde_json::Value = serde_json::from_str(raw).expect("server.json is valid JSON");
        let pkg = env!("CARGO_PKG_VERSION");
        assert_eq!(doc["version"], pkg, "server.json .version is out of sync");
        assert_eq!(
            doc["packages"][0]["version"], pkg,
            "server.json .packages[0].version is out of sync"
        );
    }
}
