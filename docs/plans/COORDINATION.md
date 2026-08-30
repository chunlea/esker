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

## Decisions

- 2026-08-30: toolchain is rustup **stable** (repo pins via `rust-toolchain.toml` in
  phase 0); `just`, `cargo-deny`, `cargo-nextest` installed via Homebrew on this machine.
- 2026-08-30: coordinator quota note — `claude` pool Fable week at 69% at start; Fable is
  reserved for review/adjudication, volume goes to Opus/Sonnet lanes.

## Incidents

(none yet)
