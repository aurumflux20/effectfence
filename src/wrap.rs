//! Wrap mode — `effectfence wrap -- <command that starts any MCP server>`
//!
//! The 10× surface: instead of asking agents to *politely* call
//! fence_prepare/fence_commit, effectfence stands in FRONT of an existing
//! MCP server and fences every tool call automatically:
//!
//! ```text
//!   agent/client ──MCP──> effectfence wrap ──MCP──> real tool server
//! ```
//!
//! - Tool list is mirrored 1:1 from the child (names, schemas, docs).
//! - Every `tools/call` derives an intent from `hash(tool + args)`:
//!   identical duplicate calls — same tool, same arguments — execute the
//!   child ONCE; later duplicates get the recorded result replayed.
//! - Concurrent identical calls: one forwards, the rest are refused with
//!   an in-flight notice (v1: no coalescing wait — callers retry).
//! - Non-identical calls pass straight through, unfenced.
//!
//! Honest v1 scope (documented, not hidden): tools only (no resource or
//! prompt passthrough yet); intent derivation treats *byte-identical
//! canonical arguments* as "the same action" — an agent that varies a
//! timestamp argument defeats dedup (that direction fails SAFE: the call
//! runs, nothing corrupts); state is in-memory per wrap process.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use rmcp::{
    ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::{RequestContext, RunningService},
    transport::{ConfigureCommandExt, TokioChildProcess, stdio},
};
use sha2::{Digest, Sha256};

use crate::fence::{Admission, EffectFence, EffectRequest, VectorClock, prepare_effect_fence};

/// Derive the intent for a tool call: SHA-256 over the tool name and the
/// JSON arguments serialized with sorted keys (via BTreeMap round-trip),
/// so key order differences don't split identical actions.
fn derive_intent(tool: &str, args: &serde_json::Value) -> String {
    fn canonicalize(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let sorted: std::collections::BTreeMap<String, serde_json::Value> = m
                    .iter()
                    .map(|(k, val)| (k.clone(), canonicalize(val)))
                    .collect();
                serde_json::to_value(sorted).expect("BTreeMap serialization cannot fail")
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(canonicalize).collect())
            }
            other => other.clone(),
        }
    }
    let canon = serde_json::to_string(&canonicalize(args)).expect("Value serialization");
    let mut h = Sha256::new();
    h.update(tool.as_bytes());
    h.update([0u8]);
    h.update(canon.as_bytes());
    format!("wrap:{}:{}", tool, hex::encode(h.finalize()))
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for b in bytes.as_ref() {
            write!(out, "{b:02x}").expect("write to String");
        }
        out
    }
}

#[derive(Clone)]
struct WrapServer {
    child: Arc<RunningService<RoleClient, ()>>,
    fence: EffectFence,
    tools: Arc<Vec<Tool>>,
    child_name: Arc<str>,
}

impl WrapServer {
    async fn forward(&self, req: CallToolRequestParams) -> Result<CallToolResult, McpError> {
        self.child
            .call_tool(req)
            .await
            .map_err(|e| McpError::internal_error(format!("child server error: {e}"), None))
    }
}

impl ServerHandler for WrapServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo (= InitializeResult) is #[non_exhaustive]: construct via
        // its constructor, then set public fields.
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new(
            format!("effectfence-wrap({})", self.child_name),
            env!("CARGO_PKG_VERSION"),
        );
        info.instructions = Some(format!(
            "Every tool of the wrapped server `{}` is fenced: an identical duplicate call \
             (same tool, same arguments) will NOT re-execute -- it returns the recorded \
             result of the first execution. A concurrent identical call is refused with an \
             in-flight notice; retry shortly to receive the recorded result.",
            self.child_name
        ));
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: (*self.tools).clone(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args_value = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
        let intent = derive_intent(&request.name, &args_value);

        let admission = prepare_effect_fence(
            &self.fence,
            EffectRequest {
                intent,
                parent: None,
                domain: format!("tool:{}", request.name),
                tool: request.name.to_string(),
                args: args_value,
                read_set: vec![],
                agent: "effectfence-wrap".to_string(),
                known_clock: VectorClock::new(),
            },
        );

        match admission {
            Ok(Admission::Fresh(prepared)) => {
                let result = match self.forward(request).await {
                    Ok(r) => r,
                    Err(e) => {
                        // Child errored before we know the outcome: free the
                        // intent so a retry may execute (fail-safe direction).
                        crate::fence::abort_effect(&self.fence, prepared, e.to_string());
                        return Err(e);
                    }
                };
                let stored = serde_json::to_value(&result)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                // Tool-level errors (isError) are NOT terminal successes:
                // free the intent so the agent's retry can execute again.
                if result.is_error.unwrap_or(false) {
                    // Tool-level errors are not terminal successes: free the
                    // intent so the agent's retry can execute again.
                    crate::fence::abort_effect(
                        &self.fence,
                        prepared,
                        "child returned a tool-level error".to_string(),
                    );
                    return Ok(result.into());
                }
                match crate::fence::commit_effect_cert(&self.fence, prepared, stored) {
                    Ok(_cert) => Ok(result.into()),
                    Err(e) => {
                        // Bookkeeping failed; the call already ran. Serve the
                        // real result -- never turn success into an error.
                        tracing_warn(&format!("wrap: commit failed: {e}"));
                        Ok(result.into())
                    }
                }
            }
            Ok(Admission::Replay(cert)) => {
                let replay: CallToolResult =
                    serde_json::from_value(cert.result.clone()).map_err(|e| {
                        McpError::internal_error(
                            format!("stored result no longer deserializes: {e}"),
                            None,
                        )
                    })?;
                Ok(replay.into())
            }
            Err(err) => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(format!(
                    "effectfence: duplicate call refused ({err}). If this was intentional as a NEW \
                 action, vary the arguments; if you are waiting on the first attempt, retry \
                 shortly to receive its recorded result."
                )),
            ]))),
        }
    }
}

fn tracing_warn(msg: &str) {
    eprintln!("[effectfence-wrap] {msg}");
}

/// Entry point: spawn the child MCP server, mirror its tools, serve fenced.
pub async fn run_wrap(child_cmd: Vec<String>) -> Result<()> {
    let (program, rest) = child_cmd
        .split_first()
        .context("wrap: no child command given. Usage: effectfence wrap -- <command> [args...]")?;

    let transport = TokioChildProcess::new(tokio::process::Command::new(program).configure(|c| {
        c.args(rest);
    }))
    .context("wrap: failed to spawn child MCP server")?;

    let child = ().serve(transport).await.context("wrap: MCP handshake with child failed")?;

    let tools = child
        .list_all_tools()
        .await
        .context("wrap: could not list child tools")?;

    eprintln!(
        "[effectfence-wrap] fencing {} tool(s) from `{}`",
        tools.len(),
        program
    );

    let server = WrapServer {
        child: Arc::new(child),
        fence: EffectFence::new(),
        tools: Arc::new(tools),
        child_name: Arc::from(program.as_str()),
    };

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
