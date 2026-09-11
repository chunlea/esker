# ADR 0108 — a cluster starts N placement drivers, and every client follows the leader

Status: accepted (phase 15) · Date: 2026-09-10
Context: `CLAUDE.md` invariants 5, 6 · `docs/DESIGN.md` §7, §15 ·
`docs/plans/phase-15-pd-ha.md` §10, §11.10 · [ADR 0011](0011-pd-service-and-the-cluster-id.md),
[ADR 0059](0059-pd-is-a-raft-group.md),
[ADR 0061](0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)

## Context — replicated in the algorithm, singular in every deployment

[ADR 0059](0059-pd-is-a-raft-group.md) made the placement driver a Raft group and
[ADR 0061](0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md) gave it dynamic
membership. `esker pd serve` takes `--id` and `--peers`; `esker pd members add|remove` runs a
change; `esker-pd`'s own tests elect, fail over and reconfigure. **The algorithm is done.**

And every cluster anybody starts still has exactly one. `esker cluster start --pd` launches a single
driver, `crates/esker-cli/src/cluster.rs` says so in the comment that restarts it —

> **What this buys is the availability of recovery, and not availability.** A single driver is still
> a single point: it is replicated in the algorithm and singular in every deployment this command
> can produce.

— and `esker-coord/h1-driver-kill.md` §2 prices what that costs: while the driver is down **no node
can start a transaction**, so no statement runs at all, reads included, and every session sees
`08006`. The exposure is total.

Two things stand between the algorithm and a cluster that survives losing its driver, and both are
named in `docs/plans/phase-15-pd-ha.md` §10 as *what another lane must add*:

1. **`esker cluster start` can only start one.** There is no `--pd-nodes`.
2. **A SQL node holds one `SocketAddr`.** `esker_sql::pd::PdConn` is the node's `RegionResolver`,
   its `TimestampOracle`, its schema lease and its columnar report, all over one address with no
   notion of a leader. `esker_store::pd_remote::RemotePd` learned to follow the leader; the SQL
   node did not, so a group that elected a new leader would leave every statement failing until the
   node was restarted — which is not high availability, it is a slower outage.

Nothing below changes a message, a format version or a golden. Every wire element this needs
already exists: `PdReq`/`PdResp`, `ProtoError::PdNotLeader { leader_address }`, and `Pd::Members`
with its `group_id`.

## Options

### Where the leader-following policy lives

The rules are four: which endpoint to believe (sticky, so a cluster pays for a leader change once);
a hint that **names** a member — move there and retry, under a budget, because a group mid-election
hands out hints that chase each other; a hint that names **nobody** — an election is in progress and
there is nothing to chase, so back off and move on rather than spin; and a hint naming an address
**outside** the list — refresh from `Pd::Members` and adopt the answer only if its `group_id`
matches the one this client first learned, which is ADR 0061's rule and what keeps ADR 0059's
protection against being routed into another cluster's driver.

They exist once already, inside `RemotePd`, woven into its dispatch thread.

* **(a) Write them again in `esker-sql`.** Cheapest, and it makes two implementations of one
  grammar. The repository has paid for that shape before; a rule that reaches only the caller it
  was written for is how the fixed family member hides the fault.
* **(b) Put the *policy* in `esker-proto`, and leave each client its own transport.** A type with
  no I/O: the endpoint list, the believed index, `group_id`, and the decision a `PdNotLeader`
  produces. `RemotePd` moves onto it; `PdConn` is built on it.
* **(c) Put it in `esker-client`.** `esker-sql` depends on `esker-client`; **`esker-store` does
  not**, and cannot — the layer table puts the client above the store. It would serve one caller.
* **(d) Have `esker-sql` use `esker-store::RemotePd`.** Wrong direction and a large dependency: a
  stateless SQL node would link the whole storage layer to learn who leads a group of three.

### How `esker cluster start` names several drivers

The supervisor writes `<data-dir>/cluster.state` as `id address pid`, one line per child, and it is
read by **four things beyond `stop`**: `esker durability chaos --state`, three test files, and
`esker-rails-harness/leader-kill.py`, which r1 wrote today and will run in run 125. Store ids are
`1..=nodes` and the driver is id `0`.

* **(e) A `kind` column.** Self-describing, and it breaks every reader listed above at once,
  including one outside this repository that a measurement run depends on this week.
* **(f) Give drivers ids out of a reserved high range.** Keeps three columns, and makes `id == 0`
  — which `durability chaos` and `restart_command` both branch on — mean "the first driver" rather
  than "a driver", which is a distinction nobody wants to hold.
* **(g) `id 0` means *a placement driver*, and there may be several; the address is its identity.**
  Three columns unchanged, the id column unchanged in meaning, and the supervisor carries each
  driver's address beside its child rather than re-deriving one from a single `Option<&str>`.

## Decision — (b) and (g)

**`esker cluster start --pd-nodes N` (default 1) founds one group of N drivers, every client is
given all N addresses, and the four rules for following the leader live once, in `esker-proto`, as
a policy with no I/O that both clients drive with their own transport.**

1. **`--pd-nodes N`**, meaningful only with `--pd`, default 1 so every existing invocation is
   unchanged to the byte. The drivers listen on the N ports **above** the stores', extending the
   rule `--pd` already follows. Each is started with `--id m --peers 1@a1,…,N@aN` — ADR 0061's
   **founding** path, the same list on every member, so the group id is derived once and written
   down. `--join` is not used: `cluster start` founds a group, it does not add to one.
2. **Every store gets `--pd a1,…,aN`**, which `esker server` already parses (`pd_endpoints`), and
   the SQL command line `start` prints carries the same list. `esker-sql`'s `--pd` learns to take
   it — one address stays valid and means a group of one.
3. **A placement driver is child id `0`, and a cluster may have several.** The state file's shape
   and its id column keep their meaning; `esker durability chaos` already skips *every* `id == "0"`
   line rather than the first; `leader-kill.py` matches census `store=N` against `id == N` and
   never sees a driver. **`--no-respawn` keeps exactly the semantics it has**: a driver that exits
   is restarted unless it is given, and it is restarted on **its own** address, which the
   supervisor now carries per child instead of deriving from one.
4. **`esker_proto::pd::LeaderBook`** — the endpoint list, the believed index, the learned
   `group_id`, and one method that turns a `PdNotLeader` into *retry here* / *back off and try the
   next* / *refresh, and adopt if the group id matches*. No sockets, no threads, no runtime: it is
   a decision, and the caller performs it. `RemotePd` moves onto it in the same unit as it is
   written, so there is never a second copy in the tree.

**Nothing changes on the wire.** No new message, no new field, no format version, no golden. This
is a client-side policy and a development command; the protocol it speaks is the one ADR 0059 and
ADR 0061 already defined.

## Rationale

**The policy is part of speaking the PD protocol, which is why `esker-proto` is not a layering
violation.** `PdNotLeader` is a wire error with an address in it; what a client must do when it
arrives is as much the protocol's rule as the encoding of the error is. `esker-proto` already holds
`PdChannel` and `BlockingTransport`, which are clients, not messages. What it must not hold is a
socket the policy owns — and it does not: `LeaderBook` never performs a call.

**`RemotePd` moves in the same unit rather than later.** A shared type with one caller and a
comment promising the second is the half that surfaces as somebody else's regression. The move is
also the cheapest test the policy will ever get: `RemotePd`'s existing tests are a written-down
statement of the rules, and they must stay green through it without being edited.

**Founding, not joining, and the same list on every member.** ADR 0061 makes the group id derived
from the founding list and then persisted, so N members handed the same list found one group. A
`cluster start` that used `--join` would have to start one driver, wait for it, add the second as a
learner, wait for it to catch up, promote it, and repeat — four round trips of a command that must
work in a test's twenty-second budget, to reach the state the founding path reaches at once. The
join path has an owner already: `esker pd members add`, which is what an operator runs on a cluster
that is *up*.

**`id 0` for several drivers is chosen against a self-describing format on purpose**, and the
reason is dated: `leader-kill.py` was written this afternoon, from `h1-run123-2501ms-gap.md`, and
run 125 depends on it. A format change that breaks a measurement tool the same week it was written
buys tidiness with somebody else's run. The id column already answers the only question its readers
ask — *is this line a store?* — and it keeps answering it.

**One correction to `docs/plans/phase-15-pd-ha.md` §10**, which prescribed these rules before
ADR 0061 landed and says *"a hint naming an address outside the list is a misconfiguration and must
not be followed"*. That is no longer true and `RemotePd` no longer does it: a driver's membership
moves, so an unknown address may simply be a member added since this client started. The rule is
ADR 0061's — refresh, and adopt only on a matching group id. The shared policy carries the current
rule, and §10's sentence is corrected in the same change rather than left to be found.

## Acceptance

The unit is not done until a **statement returns while the placement driver that was leading is
being killed**, and that is one command plus one kill:

    esker cluster start --nodes 3 --pd --pd-nodes 3 --data-dir <dir>
    esker-sql 127.0.0.1:5432 <stores…> --pd a1,a2,a3
    # load running, then:
    kill -9 <the pid of the driver that leads>

* **`crates/esker-cli/tests/`** — a `--pd-nodes 3` cluster starts, all three drivers answer
  `Pd::Members`, they agree on one leader and one `group_id`, and the state file has three `id 0`
  lines whose addresses differ.
* **The kill, in a test** — `SIGKILL` the leading driver; a `Tso` and a `GetRegion` through the
  same `PdConn` still answer, without the client being rebuilt. Red first against today's
  `PdConn`, which holds one address and cannot.
* **`esker-proto`** — the four rules as unit tests on `LeaderBook`, including a hint that names an
  address outside the list with a **mismatched** group id, which must be refused rather than
  adopted.
* **`esker-store`** — `RemotePd`'s existing tests, unedited, through the shared policy.

This also pays two of `docs/plans/phase-15-pd-ha.md` §11.10's owed items as a side effect: a
membership change over a real socket, and three drivers as three **processes** rather than three in
one, both of which `--pd-nodes 3` produces by construction.

## Consequences

* **A cluster this command starts can lose its driver and keep serving**, which is the first time
  that has been true, and the first time `h1-driver-kill.md`'s total exposure has an answer that is
  not "restart it quickly".
* **Three drivers is three more processes and three more databases** on a development box. The
  default stays 1, so nothing anybody runs today gets slower.
* **A SQL node's `--pd` becomes a list**, and a single address keeps working, so no invocation
  anywhere has to change on the day this lands.
* **`esker durability chaos` still refuses to kill a driver** — it skips `id == "0"` and that is
  right: killing the driver is a different experiment, and now it is one that can be run on purpose
  with `leader-kill.py`'s shape rather than by accident.
* **A driver's data directory is now `<data-dir>/pd-N`** rather than `<data-dir>/pd`, derived per
  member for the reason `--sst-store` derives one per node: two members sharing a database would
  each hold the other's Raft log, and the second to start would refuse to open at all. A dev
  cluster restarted on a directory from before this change finds no driver state under the new
  name and starts a fresh one; nothing outside this repository reads that path.
* **Five members remain untested**, as `docs/plans/phase-15-pd-ha.md` §7 says. `--pd-nodes 5` will
  start five and nothing here claims anything about them.
* **TLS between members is unchanged**, and `--pd-nodes` passes the same `RpcTlsFlags` every driver
  already takes.
