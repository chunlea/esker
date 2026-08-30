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

- [x] Phase 0 — scaffold (lane `wy-p0`, claude-wy/opus) — **accepted 2026-08-30 ~02:00**:
  8 commits, 115 tests, `just check` green (verified independently by coordinator),
  7/40 runtime crates; coordinator reviewed `codec.rs`, `crc32c.rs`, `deny.toml`,
  workspace manifest in full.
- [x] **Phase 1 — engine: ACCEPTED 2026-08-30 ~05:45.** Acceptance battery (Sonnet lane
  `p1-accept`): GATE PASS — 473 tests; model 10,000 cases; FaultFs 178 torn/0 lost;
  SIGKILL 1,000 iters/9,796 acks verified; concurrency 2.82M writes; bloom FP 0.88%;
  coverage region/line 92.7%/93.7% (engine 92/93, keys 99/100, base 95/94); bench
  fillrandom 342k ops/s, readrandom 378k, readmissing 3.54M (bloom 9.2×), fillseq --sync
  231 (one honest fsync per write), group commit 117×; deps 15/40; 5/5 golden formats.
  Four DESIGN drift items found and fixed (bloom-before-disk implemented, §4.8/4.1/4.2/4.6
  corrected). Coordinator reviewed codec/footer/bloom-probe/WAL-poison in full.
  One invariant-1 bug found by fault injection and fixed before any real workload.
  - [x] step 4 SST layer (`cl-p1-sst`) — **accepted 2026-08-30 ~03:00**: builder/reader/
    bloom/block/framing + 84 tests; corrupt-any-byte proves the only unchecked bytes are
    the footer's zero padding; coordinator reviewed `footer.rs` in full and the bloom
    probe path (shared `filter_key` for build+probe; extractor mismatch drops the filter).
    Contract notes relayed to the spine lane (seqno_range before finish, finish syncs,
    LevelDB cursor semantics, per-CF TableOptions parity, ADR numbering).
  - [x] `cl-p1-sst` round 2 — **accepted ~03:10**: ADR 0005, FaultFs (per-op RNG
    derivation; rename applied by fsync_dir), sst-dump. Out-of-lane edits all minimal
    and self-reported.
  - [x] `cl-p1-sst` round 3 — **accepted ~03:40**: FaultFs sweep (found the WAL bug;
    fix landed by spine in b9cb5c9, regression un-ignored as
    `a_torn_append_ends_the_segment`) + SIGKILL loop; **1,000-iteration acceptance run
    passed** (945 kills, 9,797 acked writes verified, 84.7s). The "123 unopenable" were
    cuts before CURRENT existed — not databases, nothing acked, correctly reopen-failable.
  - [x] `cl-p1-sst` round 4 — **accepted ~03:55**: full-scan kill-loop (fwd+rev must
    agree) + model test (11 ops, measured op coverage ~19.7k, failure-verified by two
    reverted mutations, artificial proptest-regressions correctly deleted). Two API
    findings relayed to spine: compaction-API canary, and **stale Snapshot accepted
    across reopen** — must be fixed inside the step-7 floor design, pinned by
    `a_snapshot_taken_before_a_reopen_is_not_rejected`.
  - [ ] `cl-p1-sst` round 5: wal-dump, manifest-dump (via public readers only),
    concurrency test (8w+8r, journals, 30s behind --ignored).
  - Standing rule updated: in a crate shared by two lanes, format with
    `rustfmt --edition 2024 <own files>` — `cargo fmt -p` still sweeps the whole crate.
  - [ ] spine: WAL ✅ batch/internal-key ✅ memtable ✅(assumed, verify at gate)
    manifest/Version in progress → Db → compaction → checkpoint → cli/bench.
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

- 2026-08-30 (phase-0 gate): lane judgment calls **approved** — `clippy::unwrap_used`/
  `expect_used` as workspace lints (invariant 9 mechanized); `RUSTDOCFLAGS=-D warnings`
  in `just doc`; `esker-base` as 11th crate (ADR 0004; CLAUDE.md table row added);
  CI third-party actions (taiki-e/install-action, Swatinem/rust-cache) accepted as
  CI-only supply chain, not in the artifact dependency graph — revisit if CI hardening
  ever matters.
- 2026-08-30: phase-1 parallelization — SST/cache are leaf modules with no dependency
  on WAL/memtable, so prompt-01's "steps in order" is honored *within the spine lane*;
  the SST lane builds pure byte-level modules against contracts pinned in both briefs.
  Contract owner (fs/dbformat/lib.rs): spine lane.

## Incidents

- 2026-08-30 ~07:00 **TEST HOLE (found by cl-p2-client)**: phase-1 `crash_kill.rs`
  ACK parser never matches the first ACK (libtest glues its line to the child's), so
  **operation 0 was never verified** in the accepted 1,000-iteration runs. Containment
  assertion ⇒ weakened, not broken. Fix + tightened assertion + 200-iteration re-run
  assigned to `cl-p2-client` under a one-time phase-1-test grant.
- 2026-08-30 ~06:50: `cl-p2-client`'s crash test hung the sibling's workspace test run
  for 8 minutes via a no-deadline pipe read (self-reported, orphans killed, fixed in
  d8a7b25 with a 30s deadline). Standing note: any blocking read in a test gets a
  deadline.
- 2026-08-30 ~06:40 **ENGINE FINDING (phase-2 lane)**: DESIGN §4.7 claimed v1 rejects
  multi-SST DeleteRange; the engine actually accepted every range and silently treated
  it as a point delete at `begin`. Store works around it (ADR 0006: bounded scan +
  point deletes, atomic). Ruling: engine now REFUSES DeleteRange in v1 with a typed
  error until phase-5 range tombstones; fix + regression assigned to `wy-p2-proto`
  under a one-time engine-write grant.
- 2026-08-30 ~04:50: watcher blind spot — a lane whose turn ended at 4:23 with a leftover
  shell kept reporting `working`, so the coordinator missed ~25 idle minutes. Watcher now
  also detects the "· done H:MM" footer. Lesson: agent_status alone does not mean the
  agent is busy.
- 2026-08-30 ~03:24 **ENGINE BUG (found by the crash sweep, before any real workload)**:
  after a partial WAL append error, the log writer kept accepting writes past the torn
  bytes → acknowledged writes after a mid-log tear are lost on recovery (invariant 1).
  At `9409bc1`: 178/240-op schedules tore, 162 unopenable, 35 acked-then-lost. Fix
  direction relayed to spine (LevelDB-style sticky background error + tail-only tears);
  repro committed `#[ignore]`d in `tests/crash_faultfs.rs`; DoD = repro un-ignored as
  regression + both sweep counts to 0. Spine's in-flight tree already had the ack half
  fixed (0 acked-after-tear) but 123 schedules still unopenable — root cause required.
- 2026-08-30 ~03:08: `cl-p1-sst` ran `cargo fmt --all`, reformatting the spine's
  in-flight files (whitespace only, self-reported). Standing rule added to all future
  briefs: **`cargo fmt -p <crate>` only** in a shared tree.
- 2026-08-30 ~01:50: found unsent text "开始 phase 1" typed into lane `wy-p0`'s input
  box (origin unknown, likely the user pre-sleep). Not submitted; phase gating held.
  Cleared/retired with the pane after acceptance. Rule reaffirmed: phases open only
  through the coordinator's gate.
