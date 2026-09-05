# 0075 — The oracle captures live in the repository

**Status**: accepted · **Date**: 2026-09-05

## Context

Every parity corpus under `crates/esker-sql/tests/corpus/` records what PostgreSQL 19 answered.
Those answers were read from a live 19beta1 and written into a **capture** — a file of statements
and their measured results — and the corpus is derived from it. Until now the captures lived
outside this repository, in the Rails harness's working directory.

Two things follow from that, and both are defects.

**A declared divergence's expected value could not be traced.** The harness compares this node
against a corpus row on every run, so a wrong expected value fails loudly — *except* for a declared
divergence, where it checks only that the two still differ. That one number is unverified by
construction, and `docs/plans/divergence-provenance.md` records how it went wrong: a corpus row
claimed PostgreSQL concatenated two `json` documents as text, written from memory, green until
somebody asked the oracle. The fix is for a divergence to cite `captures/<file>:<line>` and for the
harness to **resolve** the citation — which it cannot do if the file is not in the tree the gate
mounts. The gate container mounts `/work`, `/target` and the cargo registry, and nothing else.

**The captures were not backed up.** They were never in git. The corpus copy in this repository was
the only surviving record of a measurement, and a corpus is a *subset* — a capture routinely holds
statements no corpus took. Losing that directory would have lost oracle recordings that cost real
time on a real server to make.

## Decision

The captures are vendored into `crates/esker-sql/tests/captures/`, and that path is the single
source.

88 files, 836K, plain UTF-8 text: statements and the answers a real PostgreSQL 19beta1 gave them.

They are **test fixtures**, which is why this is a smaller decision than it looks. They are not a
dependency (nothing links them), not an on-disk or wire format (nothing parses them at run time),
and they add nothing to the crate budget `deny.toml` enforces. What they are is the evidence behind
every expected value in the corpora.

The out-of-repo directory stops being referred to. Three doc comments named it and now name the
vendored path, so there is one location rather than two that drift apart.

## Consequences

* A declared divergence can cite its measurement, and the harness can resolve the citation during
  a gate — a reference that has drifted off its line fails the way a stale expected value should.
  That is what makes the rule a check rather than a convention.
* A capture is durable, reviewable and diffable. Re-taking one against a newer PostgreSQL becomes a
  visible diff instead of a silent overwrite, which is what a parity project wants when the oracle
  itself moves.
* The repository grows by 836K of text. This is the cost, it is paid once, and it is a fifth of
  what a single `Cargo.lock` churn costs in a year.
* A capture is **trusted, not checked**: it is a recording of a real server. Nothing here verifies
  that a capture is right, only that an expected value can be traced to one. The failure this
  removes is a number written from memory, which is the only kind that survives a green suite
  indefinitely.
* Captures taken from now on are written here directly. A capture that exists only in a lane's
  scratchpad dies with the session — which had already happened to the `||` measurements this
  decision was written alongside.
