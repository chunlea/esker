# Phase-4 acceptance, re-measured after the promotion-stall fixes

Run on a quiet machine (other lanes stood down), 2026-08-31. Load average is recorded with every
number because this box is shared and the first acceptance run was taken at load 14+ on 16 cores.

## Methodology, and what a single box can and cannot show

**Single-box scale-out is inherently confounded.** Five "stores" here are five processes sharing one
disk and sixteen cores, so adding a store adds contention for the same I/O and the same CPU at the
same time as it adds capacity. Nothing measured this way can demonstrate linear scale-out, and a
number that looked linear would be evidence of a mistake rather than of scaling.

So the honest claim is **per-configuration numbers with their load context**, never a scaling curve.
What the configurations *can* show is the shape of the failure: a cluster where added stores carry
no load looks very different from one where they carry load and contend for one disk.

Every run records the load average before and after, and the region count the measured pass ran
against — throughput at two regions and at sixteen are not the same measurement.

## Repair (`run_repair.sh`): 3 stores, kill one, add a fourth

| | before the fixes | after |
|---|---|---|
| state before the kill | every region **1 voter**, all leaders on store 1 | every region **3 voters** on stores 1/2/3, leaders spread |
| regions fully repaired | 0 of 6, at 4 min | 2 of 6, at 7 min |
| acknowledged writes readable | all | 11422/11422, 0 missing |

The before-the-kill state is the headline: that is the promotion stall gone. The repair itself is
better and not yet complete — `conf_ver` reaches 10 per region, which is five membership changes
where two would do, and some regions end holding a learner on a store that already had a healthy
replica removed from it. That churn is a placement decision, and the store carries it out as asked.

## Balance (`run_balance.sh`): 5 stores, 16 regions

| | before the fixes | after |
|---|---|---|
| replicas | every region 1 voter + 2 learners | every region **3 voters** |
| leader spread | **all 16 on store 1** | **4 / 4 / 4 / 4** across stores 2–5 |
| replica spread | — | 12 / 12 / 12 / 12, `gap=0` |
| acknowledged writes readable | all | 7956/7956, 0 missing |

The script reports "did not fully converge" on one criterion only: **store 1 ends with no replica at
all**, so four stores hold everything. The spread across those four is exact. Why balance empties
the bootstrap store rather than levelling across five is a placement question, recorded here as
evidence rather than diagnosed.

## Scale-out: **not measurable yet**, and the reason is new

| stores | measured ops/s | regions in the measured pass | load before |
|---|---|---|---|
| 1 | 263.2 (p50 25 ms, p99 69 ms) | 8 | 2.85 |
| 3 | 67.6 (p50 100 ms, p99 294 ms) | 2 | 2.49 |
| 5 | run did not complete | — | 4.20 |

**These numbers are not comparable and must not be quoted as a scaling result.** Each configuration
entered its measured pass with a different amount of data and therefore a different number of
regions — 8, 2, and unknown — because the *warm-up* pass lost writes:

    p4loadgen warm-up, 3 stores: 10652 of 15000 puts failed
        5527  failed: request not sent: no address known for store 3
        5121  failed: request not sent: no address known for store 2

The load generator's store address book is filled once and never refreshed from the `GetRegion`
answers it receives, so it cannot reach a region that has moved to a store it did not know about at
startup. PD does return every peer's store and address in that answer
(`Pd::get_region` reads a `StoreRecord` per peer), so the addresses are on the wire and simply not
picked up.

**This gap was unreachable before the fixes.** While every region kept its only voter on store 1,
the generator never needed a second address. Making regions actually spread is what exposed it —
the same shape as `tests/promotion.rs` writing only to store 1, which passed for exactly as long as
the bug it was meant to catch was present.

Scale-out is worth re-running once the harness refreshes its address book, with the region count
held equal across configurations so the comparison means something.
