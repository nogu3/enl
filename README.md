# enl

> A stateless, one-shot CLI for [ECHONET Lite](https://echonet.jp/english/) — the Japanese smart-home protocol.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)

`enl` talks to ECHONET Lite devices and nothing else. It opens a socket, sends a UDP frame, collects responses with a timeout, and exits. No daemon, no state, no scheduler.

It's built to be **AI-native and UNIX-friendly**: `stdout` is pure structured JSON (one command = one JSON object), so `jq` and LLM function-calling can consume it directly. Diagnostics and machine-readable errors go to `stderr`. Distinct exit codes let `cron` / `n8n` branch on the outcome.

## Why

Heavy smart-home stacks (e.g. Home Assistant) hide their data model and offer poor observability. `enl` takes the opposite approach — a thin, transparent tool that does ECHONET Lite well and lets external orchestration (cron, n8n, scripts) compose it. It is the first in a planned family of lightweight per-protocol smart-home CLIs.

## Design principles

- **Stateless / one-shot** — open socket → send → collect with timeout → exit. No daemonization.
- **`stdout` is pure structured JSON only** — no logs, progress, or decoration mixed in.
- **Diagnostics to `stderr`** — `tracing` structured logs + machine-readable error JSON.
- **Never breaks on unknown devices/EPCs** — unknown classes or EPCs are returned losslessly as raw hex and the command still succeeds. The decode dictionary is *additive*, never required.

See [CLAUDE.md](./CLAUDE.md) for the full rationale (why no tokio, why a hand-written codec, etc.).

## Install

### From source (requires the Rust toolchain)

Tasks are defined with [Task](https://taskfile.dev) (`task` lists them all).

```bash
task build            # release build → target/release/enl
```

### Docker (no local toolchain)

Port 3610 must be owned by the process, so **host networking is required** (a bridge network can't receive device responses; `discover` also uses a per-host unicast CIDR sweep).

```bash
task docker:build     # build the runtime image
task docker:run -- discover
```

> ⚠️ If an ECHONET integration (Home Assistant, etc.) holds port 3610, it will steal the responses. Stop it while testing.
> Sample IPs use the RFC 5737 documentation range `192.0.2.0/24` — replace them with your real device IPs.

## Usage

```bash
enl discover                              # find nodes on the LAN
enl get 192.0.2.10 013001 80             # read (home AC, 0x80 operation status)
enl set 192.0.2.10 013001 80 30          # write (turn ON)
enl describe 192.0.2.10 013001           # introspect the property map
RUST_LOG=debug enl discover              # send diagnostics to stderr
```

`epc` accepts hex (`80`) or a canonical name (`power` / `operation_mode` / `open_close_state` …); names resolve class-specific first. The `set` `edt` also accepts a hex value (`42`) or an enum's semantic name (`close` / `on` …) — use hex for numeric or multi-byte values.

```bash
enl get 192.0.2.10 013001 power operation_mode room_temperature  # read by name
enl set 192.0.2.10 026301 open_close_operation close             # close the shutter by name
```

Every binary value always includes `edt_hex`; `value` is added when the decode dictionary knows it, and `name` when the EPC name is known.

## Subcommands & output schemas

- `discover [--timeout-ms 3000]` — `{"devices":[{"ip","count","instances":[...]}]}`
- `get <ip> <eoj> <epc...> [--timeout-ms 2000]` — `{"ip","eoj","esv","properties":[{"epc","name?","pdc","edt_hex","value?"}]}`
- `set <ip> <eoj> <epc> <edt> [--timeout-ms 2000]` — `{"ip","eoj","esv","result":"accepted","properties":[...]}`
- `describe <ip> <eoj> [--timeout-ms 2000]` — `{"ip","eoj","esv","get_map":[{"epc","name?","values?"}],"set_map":[...],"inf_map":[...]}`. `values` lists the value range of enum-typed EPCs (`{"41":"open","42":"close",...}`); numeric / unsupported EPCs omit it.
- `raw <ip> <deoj> <esv> [epc[:edt]...] [--seoj 05FF01] [--timeout-ms 2000]` — send an arbitrary ESV/EPC/EDT frame. `{"ip","sent_hex","response_hex","frame?":{...}}`. SNA is returned as `response_hex` rather than an error (a debugging / unsupported-op escape hatch); a `parse_error` is included if the response can't be parsed. EPC/EDT are hex-only here.

```bash
enl raw 192.0.2.10 013001 62 80          # raw Get 0x80
enl raw 192.0.2.10 013001 61 80:30       # raw SetC 0x80=ON
```

## Exit codes

Designed so `cron` / `n8n` can branch on the result.

| code | meaning |
|---|---|
| 0 | success |
| 2 | CLI argument error (clap default) |
| 3 | timeout (no response) |
| 4 | device rejected (SNA) |
| 5 | network / bind failure |
| 1 | other unexpected error |

## Development

```bash
task test          # tests, incl. codec round-trips
task clippy        # lint (-D warnings)
task fmt           # rustfmt
task check         # CI equivalent (fmt:check + clippy + test)
task docker:test   # tests inside Docker (no toolchain needed)
```

## Project layout

- `src/codec.rs` — frame data model + parse/build. Hand-written, zero-dependency. Round-trip tests guard against parse/build asymmetry bugs.
- `src/properties.rs` — optional decode layer, incl. the property-map parser (two encodings for ≤15 vs ≥16 properties).
- `src/net.rs` — UDP socket layer (owns `0.0.0.0:3610`). `discover` is a CIDR sweep (unicast Get to each host).
- `src/commands.rs` — discover / get / set / describe / raw.
- `src/error.rs` — machine-readable errors + exit codes.
- `src/main.rs` — clap CLI.

## Roadmap

The core (discover / get / set / describe) is verified against real devices.

- [x] Expanded decode dictionary — `82` spec version, `8A` manufacturer code (major vendors named, unknown ones left as hex), electric shutter `0263`, home AC `0130`. Unknown EPCs still return raw hex.
- [x] `raw` subcommand — send arbitrary ESV/EPC/EDT, return raw response hex.
- [ ] INF notification listener — pick up device-initiated state-change notifications (ESV `0x73`) via blocking recv for a window.
- [ ] Output schema stabilization — don't break across versions, since LLMs / `jq` depend on it.

## License

[MIT](./LICENSE)
