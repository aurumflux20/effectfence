#!/usr/bin/env python3
"""fencescan — find MCP tools that could fire the same effect twice.

WHAT THIS IS
------------
A *candidate finder*, not a prover. It reports evidence and refuses to
render a verdict, because an outsider reading a repo usually **cannot**
prove a double-fire: the guard often lives in a service the repo calls,
or in a sibling SDK, and a tool whose name sounds like a write may only
return a payload for someone else to sign.

Every claim this produces must be confirmed by reading the code before
it is said to anyone. A wrong public accusation costs far more than a
missed lead.

WHY IT WAS REWRITTEN (Aug 7 2026)
---------------------------------
v1 hand-verified at 3/7. Its four failure modes, each now addressed:

  1. CONTEXT BLEED — v1 took a flat 900-char window after a tool name and
     searched it for effect words, so a read tool declared next to a write
     tool inherited the write's vocabulary. This flagged `zunivo`'s
     `resolve` and every read in `hubspot-conversations-mcp`
     (`GetInboxDetails`, `ListChannels`, `RetrieveActorDetails`).
     → Now the description is bound to its own tool by brace matching,
       and read-verbs veto effect-verbs.

  2. NAME ≠ BEHAVIOUR — `XRPName`'s tool sounded like a payment but only
     returns a link for the user to sign; it never broadcasts.
     → A tool is only a candidate if its handler contains an actual write
       (HTTP POST/PUT/PATCH/DELETE, or a known mutating SDK call), and the
       write's file:line is reported as evidence.

  3. GUARDS OFF-REPO — `CryptoAPIs` was only provable after finding their
     separate public buyer SDK, which exposes an idempotency key the MCP
     server never passes.
     → Output always carries an explicit unknowns block naming what this
       tool cannot see. It never says "unguarded", only "no guard found
       in this repo".

  4. NO VERDICTS — v1 printed "AT RISK", which reads as an accusation.
     → There is no verdict field any more. Only evidence and next steps.
"""
import re, sys, pathlib, json

CODE = {".ts", ".js", ".mjs", ".py", ".go", ".rs"}
SKIP = ("node_modules", ".git", "dist", "build", "target", "__pycache__",
        "test", "tests", "__tests__", "spec", "e2e", "examples")

# Verbs that describe causing an effect in the world.
EFFECT = re.compile(r"\b(pay|payment|charge|transfer|payout|refund|settle|invoice|spend|"
                    r"send|sms|email|dispatch|notify|publish|submit|order|purchase|buy|sell|"
                    r"swap|mint|withdraw|deposit|create|deploy|provision|launch|terminate|"
                    r"delete|destroy|update|write|execute|trigger|enroll|register)\b", re.I)
MONEY  = re.compile(r"\b(pay|payment|charge|transfer|payout|refund|settle|invoice|spend|"
                    r"usdc|wallet|billing|purchase|buy|sell|swap|mint|withdraw|deposit|"
                    r"escrow|x402|budget|checkout)\b", re.I)
# If a tool says it reads, believe it over an incidental effect word.
READ   = re.compile(r"\b(get|list|read|fetch|search|query|lookup|resolve|retrieve|check|"
                    r"describe|show|view|find|count|status|inspect|validate|verify)\b", re.I)
# Evidence that the handler actually mutates something.
# Writes are almost never inside the tool declaration — they sit in a shared
# helper (DevsJony posts from a `call()` at index.js:21, reached by every tool).
# So write sites are collected PER REPO and reported as context, never bound to
# a specific tool. Claiming "this tool writes" needs a human reading the handler.
# Note `method:` may be a ternary (`method: body ? "POST" : "GET"`), so the
# quote does not follow the colon — match the verb anywhere on the line.
WRITE_CALL = re.compile(
    r"(\.post\s*\(|\.put\s*\(|\.patch\s*\(|\.delete\s*\(|"
    r"requests\.(post|put|patch|delete)|axios\.(post|put|patch|delete)|"
    r"httpx\.(post|put|patch|delete)|method\s*[:=][^,;\n]*(POST|PUT|PATCH|DELETE)|"
    r"\.create\s*\(|\.send\s*\(|\.submit\s*\(|\.execute\s*\(|"
    r"\.sign(AndSubmit|Transaction)?\s*\(|submitTransaction|sendTransaction|"
    r"\.insert\s*\(|\.save\s*\()", re.I)
# NO \b ANCHORS. `deriveIdempotencyKey` has no word boundary before "Idempotency",
# so an anchored pattern silently misses every camelCase guard — which is most of
# TypeScript, i.e. most of this market. That exact bug made v1 accuse Devotel of
# lacking idempotency while they had a whole `src/idempotency.ts` module.
GUARD = re.compile(r"(idempot|dedup|alreadySent|already_sent|alreadyPaid|already_paid|"
                   r"alreadyProcessed|already_processed|exactly[- ]?once|effectfence|"
                   r"once\.run|seenKeys?|seen_keys?|seenIds?|seen_ids?|"
                   r"processedIds?|processed_ids?|replayProtect|replay_protect|"
                   r"requestId|request_id|clientToken|client_token|"
                   r"transactionKey|transaction_key|dedupeKey|dedupe_key)", re.I)
# Retrying a non-idempotent write is its own double-fire route.
RETRY = re.compile(r"\b(retry|retries|backoff|max_?attempts|reattempt)\b", re.I)

STRLIT = re.compile(r'''"[^"]*"|\'[^\']*\'|`[^`]*`''')
TOOLNAME = re.compile(
    r"""(?:name:\s*["']([a-z0-9_.\-]{3,60})["']"""
    r"""|@mcp\.tool\(\s*\)?\s*(?:\n\s*)?def\s+([a-z0-9_]{3,60})"""
    r"""|Tool\(\s*["']([a-z0-9_.\-]{3,60})["'])""", re.I)


def _own_block(txt: str, start: int, limit: int = 4000) -> str:
    """Return only THIS tool's declaration, by brace matching.

    v1 used a flat 900-char window, which ran past the end of one tool
    into the next and mixed their vocabularies together. That single bug
    produced most of v1's false positives.
    """
    i = txt.find("{", start)
    if i == -1:
        return txt[start:start + 300]
    depth, j, end = 0, i, min(len(txt), i + limit)
    while j < end:
        c = txt[j]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return txt[start:j + 1]
        j += 1
    return txt[start:end]


def scan(root: pathlib.Path):
    tools, guards, retries, writes = {}, [], [], []
    for p in root.rglob("*"):
        if not p.is_file() or p.suffix not in CODE:
            continue
        if any(d in [q.lower() for q in p.parts] for d in SKIP):
            continue
        try:
            txt = p.read_text(encoding="utf-8", errors="ignore")
        except Exception:
            continue
        rel = str(p.relative_to(root))
        for m in TOOLNAME.finditer(txt):
            nm = next(g for g in m.groups() if g)
            block = _own_block(txt, m.start())
            if not re.search(r"(description|handler|inputSchema|input_schema|callback|execute)",
                             block, re.I):
                continue
            if nm in tools:
                continue
            tools[nm] = {
                "tool": nm, "file": rel,
                "line": txt[:m.start()].count("\n") + 1,
                "block": block,
            }
        for i, line in enumerate(txt.splitlines(), 1):
            code_only = STRLIT.sub("", line)       # a guard must be code, not prose
            if line.strip().startswith(("//", "#", "*", "/*", "/**")):
                continue
            if GUARD.search(code_only):
                guards.append({"file": rel, "line": i, "code": line.strip()[:120]})
            if RETRY.search(code_only):
                retries.append({"file": rel, "line": i, "code": line.strip()[:120]})
            # Writes are matched on the RAW line, not `code_only`: the HTTP verb
            # is itself a string literal (`method: body ? "POST" : "GET"`), so
            # stripping literals first deletes the very evidence we need.
            if WRITE_CALL.search(line):
                writes.append({"file": rel, "line": i, "code": line.strip()[:120]})

    candidates = []
    for t in tools.values():
        blk, nm = t["block"], t["tool"]
        # A read-verb in the NAME vetoes effect words anywhere in the block.
        if READ.search(nm) and not MONEY.search(nm):
            continue
        if not (EFFECT.search(nm) or EFFECT.search(blk)):
            continue
        # A write in the SAME FILE is corroboration, not proof the tool writes.
        t["writes_in_same_file"] = len([w for w in writes if w["file"] == t["file"]])
        t["money"] = bool(MONEY.search(nm) or MONEY.search(blk))
        t.pop("block")
        candidates.append(t)

    # Strongest first: money + a visible write.
    candidates.sort(key=lambda c: (c["money"], c["writes_in_same_file"]), reverse=True)
    return tools, candidates, guards, retries, writes


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit("usage: fencescan.py <path-to-repo>")
    root = pathlib.Path(sys.argv[1])
    tools, candidates, guards, retries, writes = scan(root)

    print(json.dumps({
        "repo": root.name,
        "summary": {
            "tools_declared": len(tools),
            "candidates": len(candidates),
            "write_sites_in_repo": len(writes),
            "money_candidates": len([c for c in candidates if c["money"]]),
            "guard_sites_found_in_this_repo": len(guards),
            "retry_sites_found_in_this_repo": len(retries),
        },
        # Deliberately NOT a verdict. These are leads to read, nothing more.
        "candidates": candidates[:10],
        "guards_found": guards[:5],
        "retry_sites": retries[:5],
        "write_sites": writes[:5],
        "retry_note": ("Retry logic that does not branch on HTTP method will retry writes. "
                       "If these sites wrap a create, a timeout can land the effect twice."
                       if retries else None),
        "unknowns_this_tool_CANNOT_see": [
            "Whether the API being called deduplicates server-side.",
            "Whether a guard lives in a sibling repo or a separate SDK "
            "(CryptoAPIs' idempotency key lived in a different published package).",
            "Whether a tool that looks like a write only returns a payload for "
            "someone else to sign (XRPName did exactly this).",
            "Whether the package is actually used by anyone — check downloads first.",
        ],
        "how_to_use_this": [
            "1. Open each candidate's file:line and confirm the handler really writes.",
            "2. Ask whether a retried call reaches the same effect twice.",
            "3. If a guard exists off-repo, this tool cannot see it — go look.",
            "4. Assert only what is visibly client-side. Otherwise ask a question.",
        ],
    }, indent=1))
