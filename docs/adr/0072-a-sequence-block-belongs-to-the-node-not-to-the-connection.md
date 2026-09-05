# 0072 — A sequence block belongs to the node, not to the connection

## Context

`nextval` reserves [`esker_sql::catalog::SEQUENCE_BATCH`] values in a transaction of its own and
serves them from memory. The batch exists so that a durable counter bump costs one write per 32
rows instead of one per row: the counter is a single key, and one bump per row would make every
concurrent insert into a table contend on it across the cluster.

The block is held on the `Executor`, and an `Executor` is **one per connection**. That is invisible
to a client that uses one connection and glaring to one that pools. `ActiveRecord`'s default pool
is five, so five inserts land on five connections and five blocks:

| | this node | PostgreSQL |
|---|---|---|
| five inserts on a pooled client | `1, 33, 65, 97, 129` | `1, 2, 3, 4, 5` |
| `last_value` after them | 160 | 5 |

Measured by r1 on the run-82 node, and `crates/esker-sql/tests/real_backend.rs`
`consecutive_serial_ids_are_consecutive` pins the other half: on **one** connection the ids already
are consecutive, in-process and on a real three-store cluster. So nothing about the allocator is
wrong; what is wrong is the scope it is held at.

It costs a real Rails test. `range_test.rb#test_infinity_values` reads `PostgresqlRange.first` and
the fixtures hold ids 101-105, so a created row sorts before them while the blocks are small and
after them once they have run past 105 — the flip-flop the board recorded across runs 73, 74, 75
and 77.

## Options

1. **`SEQUENCE_BATCH = 1`.** PostgreSQL's own default, and its exact numbers for every client
   shape. It also gives up the thing the batch was for: a durable, cross-region write per row,
   on the one key every writer to that table shares.
2. **Leave it.** The divergence is declared and `CACHE 32` is a thing PostgreSQL has. But nobody
   *asks* for `CACHE 32` here — it is the default and the only setting — and a pooled client is
   the normal client, not an edge case.
3. **Move the block to the node.** One allocator per process, shared by every session on it. A
   pooled client on one node sees `1, 2, 3, 4, 5`; a second node starts a new block.

## Decision

**Option 3.** The reserved block moves from the `Executor` to a node-wide allocator, joined the way
the advisory-lock table already is: `Executor::sharing_sequence_blocks(Arc<sequence::Blocks>)`,
defaulting to a private one so a standalone `Executor` — every in-process test — behaves as it does
today and the parity replay stays honest.

* the map is keyed by **`(tenant, sequence id)`**, because a node serves every database in the
  cluster and two tenants' sequences are two counters;
* a session that finds no value takes the node's lock, allocates a batch in its own transaction and
  serves the first value under the same lock, so two sessions racing on one sequence take **one**
  batch between them rather than two;
* `currval` and `lastval` stay **per session**, because that is what they are on a real server: a
  value this session last took, not one the node did;
* forgetting a block — `DROP TABLE`, `TRUNCATE … RESTART IDENTITY` — clears the node's entry, not a
  connection's. A block that outlived its sequence was already a leak and is fixed
  (`Executor::forget_sequence_block`); at node scope, leaving one would be a leak shared by every
  session on the node.

**Why not `CACHE 1`**, which would be simpler and exactly PostgreSQL: the batch is not a
micro-optimisation, it is what keeps a sequence from being a cluster-wide serialisation point.
Every `nextval` bump is a transaction against one key, which in this system means a Raft round trip
to whichever store holds it; at `CACHE 1` a table's insert rate is bounded by that key's commit
latency no matter how the rows are spread. The batch buys a factor of 32 on that bound. What made
it *visible* was the scope, not the size, and the scope is what this changes.

## Consequences

* **A pooled client on one node sees PostgreSQL's numbers**, which is the client shape that
  matters and the one the suite has. `range_test.rb#test_infinity_values` stops depending on
  order.
* **Across nodes there is still a gap**, and it is now the whole of the declared divergence:
  `SEQUENCE_BATCH` is a *cross-node* cost rather than a per-connection one. A client that spreads
  its pool over two SQL nodes sees two blocks, exactly as `CACHE 32` on a real server would across
  two backends that each cached.
* **A node that dies loses its unreserved tail**, as before. The gap a crash leaves is unchanged in
  kind and smaller in expectation, because there is one block in flight per node rather than one
  per connection.
* **The allocator is shared mutable state in a process that is otherwise per-session**, which is
  the thing to be careful about: it takes a lock, and the lock is held across a transaction. That
  is the same shape `crate::advisory::Locks` already has and the same one it is reviewed as — the
  hold is bounded by one small write, and nothing under it takes another lock.
* If a `CACHE` clause is ever written per sequence, this is where it lands: the size becomes a
  property of the sequence record and the scope stays the node's.
