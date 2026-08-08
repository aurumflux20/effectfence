# effectfence

**MCP server that stops agents double-firing side effects. 1,000 racing duplicates, exactly one execution — proven on every commit.**

```bash
npx effectfence
```

No Rust toolchain. No build step. No install-time downloads.

## Add it to your MCP client

```json
{
  "mcpServers": {
    "effectfence": {
      "command": "npx",
      "args": ["-y", "effectfence"]
    }
  }
}
```

## What it does

An agent that retries a tool call after a timeout does not know whether the first
attempt landed. The request reached the server, the work happened, the response
never came back. Nothing failed loudly — it succeeded twice.

EffectFence sits in front of those effects. Identical intents are admitted once;
every duplicate is fenced, replayed from the original result, or refused. The
guarantee is enforced under real contention, not assumed:

- **1,000 racing duplicates → exactly one execution.** Run
  `cargo run --release --example storm` against the source and watch it yourself.
- CI runs that storm on a **fresh runner with no cache** on every commit.
- Simultaneity is **forced** with a real barrier, not produced by spawning
  threads quickly and hoping the scheduler cooperates.

## Platforms

| Platform | Included |
|---|---|
| macOS Apple silicon (`darwin-arm64`) | yes |
| Linux x64 (`linux-x64`) | yes |
| Linux arm64 (`linux-arm64`) | yes |
| Windows x64 (`win32-x64`) | yes |
| macOS Intel (`darwin-x64`) | **no** — see below |

Every binary was built on its own native runner and made to answer an MCP
`initialize` on that platform before being published. None were cross-compiled.

**Intel macOS is deliberately absent.** GitHub retired the Intel runners, so the
only way to produce that binary would be cross-compiling it on Apple silicon —
shipping something that has never once executed. A missing download costs less
trust than a broken one. If you need it: `cargo install effectfence`.

## Why this package is ~8 MB

It bundles all four platform binaries rather than downloading the right one after
install. That is deliberate:

- **No `postinstall` script.** A tool whose entire job is guarding side effects
  should not fetch and execute code from the network while being installed.
- **Works with `--ignore-scripts`**, which is increasingly the default in CI. A
  download-on-install package silently produces a broken install there.
- **Works offline and air-gapped.**

The cost is that you download three binaries you will not run. A future release
will split these into per-platform packages selected by `optionalDependencies`,
so you fetch only yours. Correctness first, then size.

## Also available

- **Rust crate** — [`effectfence`](https://crates.io/crates/effectfence) to embed the fence directly.
- **[`once-kernel`](https://www.npmjs.com/package/once-kernel)** — the same
  exactly-once guarantee as a TypeScript library, zero dependencies, for when you
  want it inside your own code rather than as a separate MCP server.
- **[`once-kernel` on PyPI](https://pypi.org/project/once-kernel/)** — the Python
  kernel. It shares a payload hash with the TypeScript one (RFC 8785), so a
  Python service and a Node service agree about whether an operation already ran.

## Licence

MIT — same as the [Rust crate](https://crates.io/crates/effectfence) this packages.
