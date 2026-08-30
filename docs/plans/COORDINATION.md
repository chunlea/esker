# Coordination log

One coordinator (Fable, herdr pane `COORD`) directing coding lanes. Phases are gates
(`prompts/00` → `06`); a phase opens only after the previous one's acceptance passes.

## Standing policy

- **Models**: lanes write code on **Opus** (`--model opus`), test/coverage lanes may use
  **Sonnet**; the coordinator (Fable) reviews and adjudicates, writes no feature code.
  Pools: `claude-wy` first (most headroom), `claude` second. `codex` / `claude-kimi` /
  `cursor` are **not used** (user rule).
- **Git**: no signing; `git add <explicit paths>` only; `git status` before commit;
  commit every compilable unit; linear history on `main`; no push (no remote).
- **Lanes are file paths**, never feature descriptions. The shared-contract crate of a
  phase has exactly one writer.
- **Definition of done per phase**: acceptance checklist in the phase prompt passes,
  `just check` green, DESIGN.md updated, plan file updated with what changed and why.

## Task list

- [ ] Phase 0 — scaffold (lane: `wy-p0`, claude-wy/opus)
- [ ] Phase 1 — engine
- [ ] Phase 2 — single-node server
- [ ] Phase 3 — raft
- [ ] Phase 4 — multi-raft + PD
- [ ] Phase 5 — txn
- [ ] Phase 6 — sql / serverless

## Lane plan per phase (drafted up-front; adjust at each gate)

- **P0**: one lane (`wy-p0`), scaffold is indivisible.
- **P1 engine**: `wy-p1-spine` (opus/claude-wy) steps 1→3,5,6,7,8 — the serial spine, owns
  `lib.rs`+`dbformat.rs`; `cl-p1-sst` (opus/claude) step 4 leaf modules — owns `sst/**`,
  `cache/**`, `fs.rs` (FileSystem trait + fault-injecting test impl), pure modules,
  comparator passed as parameter; `p1-test` (sonnet) model test + crash loop + concurrency
  + bench + coverage after spine lands step 6. Prompt's "steps in order" holds *within* the
  spine; SST is a leaf with no dependency on WAL/memtable, so parallel is safe.
- **P2 server**: `proto`+`store` lane (opus), `client`+`cli` lane (opus, fake transport
  first — proto crate has exactly one writer), test lane (sonnet).
- **P3 raft**: `raft-core` (3a,3d) and `sim+stateright` (3b,3c) in parallel (both opus,
  different crates), then 3e integration on the core lane; chaos scripts on sonnet.
- **P4**: per sub-phase, store-side vs pd-side lanes; sim invariants on sonnet.
- **P5 txn**: store handlers lane vs client lane; bank/anomaly tests on sonnet.
- **P6**: 6a sql lane(s) and 6b tiering lane in parallel once P4 is stable.

## Decisions

- 2026-08-30: toolchain is rustup **stable** (repo pins via `rust-toolchain.toml` in
  phase 0); `just`, `cargo-deny`, `cargo-nextest` installed via Homebrew on this machine.
- 2026-08-30: coordinator quota note — `claude` pool Fable week at 69% at start; Fable is
  reserved for review/adjudication, volume goes to Opus/Sonnet lanes.

## Incidents

(none yet)
