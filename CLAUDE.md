# CLAUDE.md

A CLI tool dedicated to ECHONET Lite. The first in a series of AI-native smart-home CLIs.

> Name: **`enl`** (ECHONET Lite), finalized. **Public, standalone repository.**
> Because it is public, never include real home-device IPs, MACs, or network topology in tests/samples/commits. Use dummies such as `192.0.2.0/24` (RFC 5737 documentation range) for samples.

---

## Project goal

Escape Home Assistant's "heavy, opaque data structures, poor observability" by building a set of lightweight tools based on the UNIX philosophy. This repository is the first of them and **specializes in ECHONET Lite communication only**.

**What we will NOT build:** a monolith that embeds SwitchBot / Matter / ECHONET. Each protocol gets its own thin, independent CLI; cross-protocol orchestration is delegated to external tools (cron / n8n). This tool does one thing well: ECHONET Lite.

---

## Non-negotiable design principles (discuss before changing)

1. **Stateless / one-shot**
   No daemonization. Every command completes as "open a socket → send UDP → collect responses with a timeout → exit". No periodic execution, no state, no scheduling.
2. **stdout is pure structured JSON only**
   No human-oriented decoration, progress, or logs on stdout. `jq` and LLM function calling must be able to consume it as-is. One command = one JSON.
3. **Diagnostics go to stderr as structured logs**
   Network errors, timeouts, and device rejections (SNA) all go to stderr via `tracing` structured logs, at a granularity that makes root-cause isolation immediate.
4. **Never break on unknown devices or EPCs**
   When an unknown class or unknown EPC arrives, emit the raw hex losslessly and exit successfully. The decode dictionary is "extra info when available", never required.

---

## Tech stack

| Area | Choice | Notes |
|---|---|---|
| Language | Rust | Safety, binary size, speed |
| CLI | `clap` (derive) | |
| Networking | `std::net::UdpSocket` + `socket2` | socket2 is used only to set `SO_REUSEADDR`. **No tokio** (see below) |
| Binary codec | **Hand-written (zero deps) = final** | binrw / nom rejected (reasons below) |
| Logging | `tracing` + `tracing-subscriber` | stderr output, controlled via `RUST_LOG` |
| Serialization | `serde` + `serde_json` | |

### Why no tokio
One-shot Get/Set/Discovery completes with a single socket + a `set_read_timeout` loop. Waiting for INF notifications can be written with blocking recv. tokio only pays off for "polling many devices in parallel", and for now sequential or threaded is enough. **Since lightness is the reason this tool exists, tokio stays out until there is real demand.**

### Why the codec is hand-written (zero deps)
**Decision: hand-written. Neither binrw nor nom.**

- The frame is a simple "12-byte header + TLV-like repetition" structure; a zero-dependency hand-written codec stays robust at under ~200 lines plus tests.
- nom is parse-only. This tool needs **both a parser and a builder**, so the builder would be hand-written anyway, creating parse/build asymmetry bugs.
- binrw can declare both directions, but adds an external crate = **SemVer tracking, Dependabot PRs, supply-chain risk**. For a tool whose banner is "light and unbreakable", the frame has no complexity that justifies that dependency.
- Hand-written policy: the codec must always have round-trip tests (parse→build→parse must match). This is the rampart against asymmetry bugs in place of a declarative codec.

---

## ⚠️ The most important pitfall: port 3610

Most spec-compliant devices send replies **to port 3610, not to the sender's ephemeral source port**. To reliably receive responses you must listen on 3610.

- **Replies being fixed to 3610 is verified on real devices (2026-07-16)**: sending a unicast Get from an ephemeral source port got zero replies from every device, and tcpdump showed responses always going to 3610 rather than the source port. A design of "send from an ephemeral port and wait for the reply there" does not work.
- **Coexistence model for enl processes on 3610 (v1.5.0, verified on real devices)**:
  - `listen` binds to `224.0.23.0:3610` (the multicast group address itself) with `SO_REUSEADDR` and joins the group. This socket receives multicast only. Binding to a group address assumes Linux.
  - One-shot commands (get/set/discover/describe/raw) bind to `0.0.0.0:3610` with `SO_REUSEADDR`. Unicast replies arrive at the wildcard socket, so they coexist with `listen`. Coexistence requires REUSEADDR on **both** sides.
  - One-shots serialize among themselves via flock (`/tmp/enl-3610.lock`, a host-global fixed path). Per-user XDG_RUNTIME_DIR is deliberately not used so that cron / systemd services / manual runs share the same lock. This is needed because under REUSEADDR a second wildcard bind succeeds and the later socket steals unicast traffic (verified on real devices). Lock acquisition retries every 30ms for up to 2000ms; on exhaustion, exit 5.
  - Trade-off: `listen` cannot receive unicast-addressed INF/INFC (state-change announcements are multicast, so there is almost no practical impact).
  - A wildcard socket may receive multicast even without joining (`IP_MULTICAST_ALL` defaults to 1). One-shots skip those via the existing "expected IP + EHD/TID match" filter, so no practical impact.
- While an external process that does not set REUSEADDR (e.g. Home Assistant's ECHONET integration) holds 3610, responses are still swallowed / bind fails as before. Only on `EADDRINUSE` we retry every 30ms for up to 2000ms and then exit 5; the stderr detail distinguishes "still not released after retrying". Ultimately this CLI becomes the sole controller (consistent with the original goal).
- Bind failures and lock-acquisition failures must be distinguishable via the dedicated exit code (5).
- `set --nowait` sends SetI (0x60, no response expected) from an ephemeral port, send-only. It remains the fastest path, touching neither 3610 nor the lock. Device rejections (SetI_SNA) cannot be detected; exit 0 means "sent" only (as before).

---

## Frame structure (the core of the implementation)

```
EHD1(1) EHD2(1) TID(2) | EDATA
EDATA(normal): SEOJ(3) DEOJ(3) ESV(1) OPC(1) [EPC(1) PDC(1) EDT(PDC)]×OPC
```

The data model **must include OPC (number of processed properties) and PDC (byte length of each EDT)** — a frequent mistake that was missing from the initial requirements list.

Branches to bake into the types from the start:

- **SETGET family (ESV `0x6E`/`0x7E`/`0x5E`)**: EDATA is a two-stage structure of `OPCSet + set blocks + OPCGet + get blocks`. Separate it from normal frames with an enum.
- **SNA / error responses (ESV `0x5x`: `0x51` SetC_SNA, `0x52` Get_SNA, etc.)**: this is the "rejection from the device" in the requirements. **Distinguish it as a structured error and reflect it in all three of JSON, stderr, and exit code.**
- **EHD2 = `0x82` (arbitrary format)**: not generically parseable. Explicitly error or pass raw bytes through (choose whichever does not break).

### Keep EDT decoding in a separate layer
- **Codec core = dump layer**: always emits `raw hex + PDC` losslessly. Never breaks on unknown devices.
- **Decoding = optional layer driven by a property definition table**: when the dictionary has an entry, add a human-readable value alongside; otherwise leave the raw hex.
  This reconciles "robustness" with "AI-native" (without a dictionary, the LLM interprets the raw hex).

---

## Implementation order (Steps 1/2 of the requirements are swapped)

Discovery itself is "send a correct frame and parse the responses", so the codec is the foundation.

1. **codec**: frame data model + parser + builder (+ unit tests). ← Start here
2. **discovery**: send Get for **EPC `0xD6` (self-node instance list)** to the node profile `0x0EF001` and aggregate. Our own (controller) SEOJ is `0x05FF01`.
3. **get / set**: read and operate by IP, EOJ, and EPC.

### Strong feature to add (priority: medium)
`describe <ip> <eoj>`: property-map introspection. Query EPC `0x9F` (Get map) / `0x9E` (Set map) / `0x9D` (state-change) and mechanically present "what this device can do". Restores the observability lost in HA and hands usable properties to AI.
**Caution:** property maps switch between **two bit-encoding formats** at 15-or-fewer vs 16-or-more supported properties. A dedicated parser is required.

---

## Conventions

### stdout JSON
- On success, print the result data itself to stdout (no excessive wrapper = easy for jq).
- Document the output schema of each subcommand. Keep schemas stable (LLMs depend on them).
- Binary values always include `"edt_hex"`. Add `"value"` etc. only when decoding succeeded.

### stderr errors
- Machine-readable errors go to stderr as JSON: `{"error": {"kind": "...", "detail": "...", ...}}`.
- Example `kind`s: `timeout` / `device_rejected` / `network` / `parse` / `bind` / `usage`.

### Exit codes (split so cron / n8n can branch)
| code | meaning |
|---|---|
| 0 | success |
| 2 | CLI argument error (clap default; **never reuse for anything else**) |
| 3 | timeout (no response) |
| 4 | device rejection (SNA) |
| 5 | network / bind failure |
| 1 | other unexpected |

---

## Multicast

- Address: `224.0.23.0:3610`
- `listen` binds to `224.0.23.0:3610` (the group address itself) and calls `join_multicast_v4`. Sending commands do not join (replies come back as unicast, so joining is unnecessary; not joining also avoids our own sent frames looping back).
- `discover` always combines "CIDR sweep + one multicast probe". Multicast is ECHONET Lite's standard discovery method and works with no arguments even when the CIDR is unknown. The timeout is adjustable via a CLI flag.
- **Test-environment trap**: if it looks like "some device ignores unicast and only answers multicast", suspect the test environment first. UDP through WSL2 / Windows drops replies to ephemeral source ports (re-verification from a LAN-attached Linux host confirmed all devices answer unicast). Always verify against real devices from a directly LAN-attached host.
- `get` / `set` / `describe` / `raw` can switch only the destination to `224.0.23.0` with `--multicast`. The `ip` argument becomes "the source we expect the reply from". No automatic fallback (retrying multicast when unicast fails) — it makes duration unpredictable and hurts one-shot transparency, so it is an explicit flag. Note: a multicast Set is executed by **every device on the LAN** whose DEOJ matches (the response report covers only the `ip` device).
- Reply acceptance requires "expected IP match + EHD (0x1081) + TID match". Multicast can cross-talk with other controllers' traffic so this is mandatory, and it is applied to unicast too.
- The multicast egress interface is not controlled (left to the routing table). socket2 is already in for SO_REUSEADDR, but egress control stays out until there is real demand (YAGNI). On multi-homed hosts traffic may leave an unintended interface (add `-i`-linked egress control when real demand appears).

---

## Development commands (template)

```bash
cargo build
cargo test                 # keep the codec round-trip (parse→build→parse) tests thick
cargo clippy -- -D warnings
RUST_LOG=debug cargo run -- discover
```

---

## Things we do NOT do (reminder)

- Do not add other protocols (SwitchBot/Matter) to this binary.
- Do not add a daemon, resident process, or internal scheduler.
- Do not panic / error-exit on unknown EPCs (return raw hex).
- Do not add tokio / concurrency before real demand exists.
- Do not mix logs, progress, or decoration into stdout.
