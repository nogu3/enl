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

Port 3610 must be owned by the process, so **host networking is required** (a bridge network can't receive device responses; `discover` uses a per-host unicast CIDR sweep plus a multicast probe).

```bash
task docker:build     # build the runtime image
task docker:run -- discover
```

> ⚠️ If an ECHONET integration (Home Assistant, etc.) holds port 3610, it will steal the responses. Stop it while testing.
> Overlapping `enl` one-shots retry the bind themselves (`EADDRINUSE` only, 30 ms interval, up to 2 s), so brief collisions with cron/periodic callers resolve without the caller retrying.
> Sample IPs use the RFC 5737 documentation range `192.0.2.0/24` — replace them with your real device IPs.

## Usage

```bash
enl discover                              # find nodes on the LAN
enl get 192.0.2.10 013001 80             # read (home AC, 0x80 operation status)
enl set 192.0.2.10 013001 80 30          # write (turn ON)
enl describe 192.0.2.10 013001           # introspect the property map
enl listen --eoj 0291 --epc 80           # wait for one lighting on/off INF notification
enl schema get | jq .                    # print a subcommand's output JSON Schema (no network)
RUST_LOG=debug enl discover              # send diagnostics to stderr
```

`epc` accepts hex (`80`) or a canonical name (`power` / `operation_mode` / `open_close_state` …); names resolve class-specific first. The `set` `edt` also accepts a hex value (`42`) or an enum's semantic name (`close` / `on` …) — use hex for numeric or multi-byte values.

```bash
enl get 192.0.2.10 013001 power operation_mode room_temperature  # read by name
enl set 192.0.2.10 026301 open_close_operation close             # close the shutter by name
```

Every binary value always includes `edt_hex`; `value` is added when the decode dictionary knows it, and `name` when the EPC name is known.

## Subcommands & output schemas

All subcommands accept the global `-i <IPv4>` / `--iface <IPv4>` flag — your local IPv4 address, used by `discover` to infer a `/24` CIDR when `--cidr` is omitted and by `listen` to pick the multicast join interface.

- `discover [--cidr <CIDR>] [--timeout-ms 3000]` — `{"devices":[{"ip","count","instances":[...]}]}`. Sends a unicast CIDR sweep **plus** one multicast probe (multicast is the standard ECHONET Lite discovery method, and lets `discover` run with no arguments). With neither `--cidr` nor `-i`, the sweep is skipped and only multicast is used.
- `get <ip> <eoj> <epc...> [--multicast] [--timeout-ms 2000]` — `{"ip","eoj","esv","properties":[{"epc","name?","pdc","edt_hex","value?"}]}`
- `set <ip> <eoj> <epc> <edt> [--multicast] [--timeout-ms 2000]` — `{"ip","eoj","esv","result":"accepted","properties":[...]}`
- `describe <ip> <eoj> [--multicast] [--timeout-ms 2000]` — `{"ip","eoj","esv","get_map":[{"epc","name?","values?"}],"set_map":[...],"inf_map":[...]}`. `values` lists the value range of enum-typed EPCs (`{"41":"open","42":"close",...}`); numeric / unsupported EPCs omit it.
- `raw <ip> <deoj> <esv> [epc[:edt]...] [--seoj 05FF01] [--multicast] [--timeout-ms 2000]` — send an arbitrary ESV/EPC/EDT frame. `{"ip","sent_hex","response_hex","frame?":{...}}`. SNA is returned as `response_hex` rather than an error (a debugging / unsupported-op escape hatch); a `parse_error` is included if the response can't be parsed. EPC/EDT are hex-only here.

`--multicast` sends the frame to `224.0.23.0` instead of `<ip>`, while the response is still expected from `<ip>`. There is no automatic fallback; the flag is always explicit. The multicast egress interface is left to the routing table (a known limitation on multi-homed hosts). Beware with `set`: a multicast Set is processed by **every** device whose EOJ matches the target DEOJ, not just `<ip>` — only the reply from `<ip>` is reported.

```bash
enl raw 192.0.2.10 013001 62 80          # raw Get 0x80
enl raw 192.0.2.10 013001 61 80:30       # raw SetC 0x80=ON
```

- `listen [--count 1] [--timeout-ms 60000] [--from <ip>] [--eoj <hex>] [--epc <hex>]` — wait for INF/INFC state-change notifications (binds 3610, joins `224.0.23.0`) and exit once `count` events are collected or the timeout elapses (`0` = wait indefinitely). `{"events":[{"ip","tid","seoj","deoj","esv","properties":[...]}]}`. `--eoj` matches the source EOJ: 4 hex digits = class (`0291` = any single-function lighting), 6 = exact instance. Zero events → exit 3 (timeout), one or more → exit 0. INFC is acknowledged with INFC_Res. Still one-shot: it never daemonizes — drive it from an external loop:

```bash
# when any lighting announces a state change, turn on another light
while ev=$(enl listen --eoj 0291 --epc 80 --timeout-ms 0); do
  echo "$ev" | jq -e '.events[0].properties[0].value.power == "on"' >/dev/null \
    && enl set 192.0.2.11 029101 power on
done
```

- `schema [discover|get|set|describe|raw|listen]` — print the JSON Schema (draft 2020-12) of a subcommand's stdout output. Omit the target to get every subcommand keyed by name (`{"discover":{...},"get":{...},...}`). Stateless, no network. The output schema is a stable contract, so LLM function-calling / `jq` can fetch it programmatically.

## Exit codes

Designed so `cron` / `n8n` can branch on the result.

| code | meaning |
|---|---|
| 0 | success |
| 2 | CLI argument error (clap default) |
| 3 | timeout (no response) |
| 4 | device rejected (SNA) |
| 5 | network / bind failure |
| 1 | invalid input detected by enl (`usage`), parse error, or other unexpected error |

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
- `src/net.rs` — UDP socket layer (owns `0.0.0.0:3610`; retries `EADDRINUSE` binds for up to 2 s to absorb overlapping one-shots). `discover` is a CIDR sweep plus a multicast probe (the standard ECHONET Lite discovery method); `listen` joins the `224.0.23.0` multicast group.
- `src/commands.rs` — discover / get / set / describe / raw / listen.
- `src/schema.rs` — JSON Schema of each subcommand's stdout output (the `schema` subcommand).
- `src/error.rs` — machine-readable errors + exit codes.
- `src/main.rs` — clap CLI.

## Roadmap

The core (discover / get / set / describe) is verified against real devices.

- [x] Expanded decode dictionary — `82` spec version, `8A` manufacturer code (major vendors named, unknown ones left as hex), electric shutter `0263`, home AC `0130`. Unknown EPCs still return raw hex.
- [x] `raw` subcommand — send arbitrary ESV/EPC/EDT, return raw response hex.
- [x] Output schema stabilization — each subcommand's stdout JSON Schema is published via the `schema` subcommand and machine-fetchable, so LLMs / `jq` can pin to it across versions.

## License

[MIT](./LICENSE)
