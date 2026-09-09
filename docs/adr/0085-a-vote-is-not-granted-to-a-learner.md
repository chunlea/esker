# 0085 — A vote is not granted to a peer this configuration holds as a learner

*Status: accepted (2026-09-09). Narrows the vote grant of
[ADR 0006](0006-raft-core.md)'s core; the pre-vote of §9.6 and the leader lease of §6.2 are
untouched, and this is the rule that covers the case they leave open.*

## Context

A three-store cluster stalled four separate times over two nights, always in the same state: **a
region with no leader for tens of seconds**, and a learner beside it that was never promoted
because promotion is the leader's job. It showed up as four different assertions —

| where | region | how it showed |
|---|---|---|
| g1's gate, `95747668` | 58, a split child | three peers `PreCandidate`, terms 62/62/64 |
| h1, under load | 1 | learner un-promoted 30 s, "no store led region 1 when asked" |
| h1's gate, `7d2ead6f` | 1 | nine `NotLeader` refusals inside one call |
| h1's gate, `7d2ead6f` | 1 | never reached three voters in 60 s |

— and none of them named a cause. Three hypotheses were eliminated by measurement rather than by
reading: pre-vote term inflation (both sites are already term-neutral), the plain election path
(200 seeds from a cold start elect within 17 ticks; one voter of three lost leaves the term under
20 over 200 ticks), and tick starvation ([ADR 0081](0081-the-tick-driver-catches-up.md)'s catch-up
reported **zero** dropped ticks in a reproduction).

What found it was counters rather than a trace, and that is itself part of the finding: with
`RUST_LOG=esker_raft=debug` the stall passed **four runs of four**. Per-event tracing displaces the
window. So `esker_raft::Counters` counts campaigns, votes sent, answered and granted, responses
ignored because the round they answer is over, and `check_quorum` step-downs — an add per event,
read once at the end. One failing run, a region holding one voter and one learner:

```text
store 1 (Voter):   pre=415 real=2  answered=478 granted=477 ignored=342 step_downs=1
store 2 (Learner): pre=404 real=81 answered=111 granted=1   ignored=114 step_downs=0
```

The learner campaigned 404 times and adopted a term 81 times.

## The mechanism

Two halves, and they are on opposite sides of the wire.

**The campaigning peer's own configuration has it as a voter.** `Raft::campaign` already declines
when `!self.is_voter(self.id)`, so it is not campaigning in spite of its configuration — it is
campaigning because of it, while the placement driver and the granting store hold it as a learner.

**Whether that view is *wrong* or merely *ahead* is not settled, and this ADR does not need it to
be.** A core that has applied a promotion is ahead of the driver until the leader's next region
heartbeat, and that window is normal; an assertion that the two agree *instantly* fires on it, which
is how it was measured here. With a five-second window instead, six runs at the rung that reproduced
the stall show **no lasting disagreement** — so the lasting, wrong configuration this was first
written as is not demonstrated, and the honest statement is the weaker one: the peer's view and the
granter's differed.

The decision below does not rest on which it was. A granter can act only on its own configuration,
and by that configuration the asker could not win.

**Nothing asked whether the sender was a voter here.** §6.2's leader lease refuses a higher-term
vote request while the receiver can hear a leader, and that covers the ordinary case completely — a
test of exactly that shape passes without this change, which is why the defect was never found from
a healthy group. But a receiver that is itself **campaigning** has `leader = None`, so the veto does
not apply, and a learner's merits are good because its log is up to date.

What lives in that gap does not end on its own:

1. the voter loses its leader and pre-campaigns, so `leader = None`;
2. the learner's pre-vote arrives, is not vetoed, and is granted on its merits;
3. the learner adopts a term and campaigns for real;
4. the voter steps down to that term and pre-campaigns again;
5. and back to 1.

A learner can never reach a quorum, so no iteration of that loop can elect anybody.

## Decision

**`handle_vote_request` grants only to a peer this node's current configuration admits as a
voter.** One condition, `self.is_voter(from)`, beside the existing `can_vote` and the log's
up-to-date test.

It is not a new rule but the other side of one: `campaign` applies the same test to *itself*, and a
node that may not stand for election is a node whose vote request means nothing. Answering it cost
the granter its own vote for the term and — for a real vote — its leader, and bought nothing.

## Consequences

**A node being promoted is not harmed, and this is a proof rather than a hope.** Three facts make
it one:

1. a configuration change takes effect on a node when it **appends** the entry (dissertation §4.1),
   and the leader replicates it at once — so the window in which a promoted peer is a voter to
   itself and a learner to others is one entry's replication latency;
2. this implementation changes membership **one server at a time** (`conf.rs` refuses overlapping
   single-server changes, "which can produce two disjoint majorities"), so there is no joint
   configuration in which the new voter belongs to a half that a quorum needs. A promotion only
   *adds*: every node that has not applied it still holds the previous voter set, and that set had
   a majority before the change and still has one;
3. so an election during the window proceeds among the nodes that agree, and the promoted peer —
   refused — learns the configuration from the first append any leader sends it, after which it is
   a voter here too and its next request is granted.

The refusal therefore delays a peer's first election by at most the time its promotion takes to
arrive, and only in the window where the other voters would not have counted its votes anyway.

**A peer whose view differs becomes harmless rather than reconciled.** This is containment. A peer
that believes itself a voter will still campaign, and will still be told no by every node that does
not yet agree; what it can no longer do is take a term from a region that is trying to elect. How
the two views come apart — a lag that is expected, or a divergence that is not — is still open, and
`promotion.rs` now asserts it directly: a core and the driver disagreeing about a role for more than
five seconds fails the test where nothing noticed it before.

**Measured, either side, with a load arm proven to be applying load** (`PROMOTION_LOAD_THREADS`,
which *fails* a run whose one-minute average did not rise, because twenty earlier runs against an
arm the harness had already reaped are what made this take two nights):

| rung | without | with |
|---|---|---|
| idle | 0 of 4 failed | — |
| 6 threads | **2 of 5 failed**, 5–66 s | **0 of 18**, 6–10 s |
| 14 threads | — | 0 of 10, 4.1–14.7 s |
| 40 threads | 0 of 5 | 0 of 10, 3.8–15.5 s |

At the rung where it reproduces, the **bimodal distribution is gone** and not merely the failures:
5–66 s becomes 6–10 s, every run the fast one. A race made rarer would have left the slow mode
behind, and that difference is the argument that the mechanism is the one described above and not a
neighbour of it.

**It reproduces in a band, not with load generally**, and the last row is why that is written down:
without the fix, forty threads produced 0 of 5. Heavier load slows the learner's campaigns along
with everything else. So the rows at 14 and 40 say *no regression under load* and nothing about the
fix being necessary — the necessity rests on the six-thread rung, on the counters that named the
mechanism, and on `a_campaigning_voter_still_refuses_a_learner`, which is red without this change
and green with it in the state machine where nothing is timing at all.

**What it does not change.** Pre-vote stays term-neutral, the leader lease stays as it is, and a
vote to a voter is decided exactly as before. `Config::pre_vote` and `check_quorum` keep their
meanings.
