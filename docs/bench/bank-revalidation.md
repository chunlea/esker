# Bank sweep — re-validation on the fixed harness

`crates/esker-client/tests/bank.rs`, the `a_thousand_seeds` sweep. Plan: `docs/plans/phase-5.md`.
Not a gate; the gate runs the three-second `money_is_never_created_or_destroyed`.

## Why this run exists

The 1,000-seed zero-violation verdict on record was taken before two harness defects were known,
and it overstated what it had exercised:

* **No-kill seeds ran for half their stated duration.** The kill interval was `duration /
  kills.max(1)`, so a seed with no kills slept one interval instead of the whole run. Fixed in
  `eab588e`.
* **The auditor spent most of its attempts on moments that could not answer**, which is what this
  run found and fixed — see below. The audit is where the *sum* invariant is asserted, so a seed
  whose audits never completed asserted only the end-of-run reconciliation.

## Run — 2026-08-31

| Field | Value |
|---|---|
| commit | `97d9dcd` |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `--release` |
| machine | aarch64-apple-darwin, Darwin 27.0.0 |
| seeds | **320**, run as 320 separate processes so no failure could abort the sweep and every seed has a recorded verdict |
| shape | `Plan::one_seed`: seed % 4 == 0 → replicated, 1 leader kill, 3 s; otherwise unreplicated, no kills, 1.2 s |
| kill mix | **80 killed / 240 unkilled** — the same one-in-four the original used |
| clients | 6 per seed, 16 accounts, 10% of transfers abandoned mid-commit on purpose |

### Verdict

```text
320 seeds, 320 PASS, 0 FAIL
violations of the sum invariant .......... 0
violations of the witness claim .......... 0
committed transfers ................... 5 489   (1 543 under kills, 3 946 without)
completed snapshot audits ............. 1 056   of 1 651 attempts
seeds that completed no audit at all ...... 0
```

**Zero violations.** Every seed reconciled: every transfer the client was told had committed was
visible afterwards, and every completed audit read a total of exactly 16,000.

### Audit completion, before and against

The number that says this run exercised what it claims:

| | attempts | completed | rate |
|---|---:|---:|---:|
| unkilled seeds (240) | 715 | 715 | **100%** |
| killed seeds (80) | 936 | 341 | 36% |
| *pre-fix harness, seeds 1–8, measured* | 111 | 24 | *~22%, and one seed in eight completed none* |

The unkilled rate is 100% because an audit that cannot answer now does not happen: the auditor
waits until the accounts have existed for a full audit lag before its first attempt, instead of
spending its early attempts reading a moment before they were opened. The killed rate is 36% and
that is honest — a leader kill really does make a scan unanswerable for a while, and those
attempts are counted rather than hidden.

### The two harness defects this run found

1. **Attempts spent before the accounts existed.** The auditor reads `lag` into the past and began
   immediately, so every attempt before `opened_at + lag` audited a moment with no accounts in it —
   a complete and perfectly true audit of nothing, counted short. Pre-fix seeds reported
   "1 of 14 audits complete" with "fewest accounts seen: 0"; post-fix, no seed reports a short
   audit at all.
2. **A dead client held for five attempts.** The auditor rebuilt its connections after *five*
   consecutive unreadable attempts, and a client whose store was killed fails instantly and
   forever. A short run has only a handful of attempts, so seeds 20 and 32 spent every one of them
   on a connection that could never answer, reported `0 of 2` and `0 of 7`, and failed the
   `audits > 0` coverage assertion **with the cluster perfectly healthy**.

Killed seeds also went from 1.5 s to 3 s: measured with both fixes in, one killed seed in twelve
still reached the assertion having managed a single attempt.

## What this run proves that the original could not

The original could say that 1,000 seeds produced no violation it detected; this one can say that
**the sum invariant was actually asserted on every seed** — 1,056 completed snapshot audits across
320 seeds, with not one seed reaching its end having checked the sum zero times — and that every
seed ran for the full duration it claims. A sweep whose audits mostly failed to complete was
resting its verdict on the end-of-run reconciliation alone, which is a real claim but a weaker one:
it checks that committed transfers are visible at the end, not that money was conserved at every
instant *during* the faults.

## What it still does not prove

* 320 seeds, not 1,000. The shape and the per-seed assertions are the same; the sample is smaller.
* The sixty-second acceptance run (`sixty_seconds_of_transfers_under_faults`, 12 kills) is separate
  and was not re-run here.
* A killed seed still completes about a third of its audit attempts. Raising that means either a
  longer run or an auditor that distinguishes "no leader right now" from "this connection is dead",
  and neither is worth doing until something asks for it.

## Appendix — every seed's verdict

All 320 passed. Each entry is `seed:committed-transfers:audits-completed/attempted`;
a seed whose number is divisible by four is a replicated run with one leader kill.

```text
1:24:1/1 2:12:1/1 3:15:1/1 4:19:3/4 5:21:12/12 6:19:1/1 7:7:1/1 8:18:6/7 
9:11:4/4 10:20:8/8 11:10:1/1 12:27:5/12 13:20:1/1 14:30:1/1 15:10:1/1 16:14:8/9 
17:29:4/4 18:16:3/3 19:12:4/4 20:14:1/2 21:12:1/1 22:6:1/1 23:24:5/5 24:18:9/10 
25:13:3/3 26:23:4/4 27:31:16/16 28:14:7/10 29:21:4/4 30:2:1/1 31:21:1/1 32:13:2/9 
33:19:4/4 34:8:2/2 35:21:2/2 36:14:2/3 37:12:1/1 38:12:4/4 39:12:1/1 40:21:3/4 
41:9:1/1 42:7:1/1 43:28:7/7 44:15:1/27 45:13:1/1 46:34:1/1 47:13:1/1 48:29:8/28 
49:16:4/4 50:9:2/2 51:23:1/1 52:34:6/13 53:22:3/3 54:16:1/1 55:26:2/2 56:15:4/5 
57:10:1/1 58:14:1/1 59:15:5/5 60:15:2/2 61:23:3/3 62:10:1/1 63:8:1/1 64:16:3/4 
65:22:1/1 66:19:2/2 67:8:1/1 68:17:1/27 69:35:3/3 70:9:1/1 71:3:1/1 72:26:6/34 
73:24:1/1 74:3:1/1 75:21:6/6 76:12:1/2 77:21:1/1 78:19:2/2 79:5:1/1 80:17:6/7 
81:13:2/2 82:17:3/3 83:12:1/1 84:15:4/5 85:22:5/5 86:7:1/1 87:27:1/1 88:19:6/37 
89:15:6/6 90:18:2/2 91:14:1/1 92:10:3/4 93:12:2/2 94:10:2/2 95:22:7/7 96:19:7/8 
97:16:1/1 98:20:8/8 99:8:2/2 100:18:9/21 101:41:4/4 102:13:1/1 103:14:2/2 104:19:1/32 
105:10:2/2 106:12:1/1 107:21:2/2 108:13:2/3 109:23:4/4 110:12:3/3 111:26:1/1 112:33:8/14 
113:20:5/5 114:18:1/1 115:16:2/2 116:19:5/6 117:11:1/1 118:15:3/3 119:11:2/2 120:19:7/8 
121:24:1/1 122:21:7/7 123:12:2/2 124:25:7/8 125:14:1/1 126:9:1/1 127:11:3/3 128:23:2/3 
129:14:1/1 130:23:1/1 131:10:1/1 132:19:3/4 133:18:14/14 134:8:1/1 135:8:4/4 136:20:2/3 
137:6:2/2 138:21:3/3 139:27:11/11 140:16:7/31 141:11:4/4 142:25:3/3 143:4:1/1 144:27:2/11 
145:22:12/12 146:22:3/3 147:26:12/12 148:35:2/25 149:20:1/1 150:15:1/1 151:6:1/1 152:15:2/2 
153:6:1/1 154:11:1/1 155:23:3/3 156:17:3/34 157:11:1/1 158:17:2/2 159:13:3/3 160:22:5/6 
161:14:4/4 162:8:2/2 163:21:4/4 164:18:1/2 165:10:1/1 166:7:1/1 167:7:1/1 168:24:6/7 
169:23:1/1 170:11:1/1 171:11:5/5 172:22:2/11 173:19:2/2 174:6:1/1 175:16:1/1 176:18:2/10 
177:11:1/1 178:5:1/1 179:23:3/3 180:27:3/4 181:8:2/2 182:16:1/1 183:21:2/2 184:11:2/3 
185:23:2/2 186:9:1/1 187:20:1/1 188:14:7/9 189:24:1/1 190:6:1/1 191:27:16/16 192:24:2/3 
193:6:1/1 194:5:1/1 195:20:1/1 196:17:5/6 197:19:6/6 198:2:1/1 199:15:2/2 200:15:1/12 
201:20:7/7 202:33:2/2 203:5:2/2 204:18:2/3 205:29:2/2 206:19:6/6 207:9:1/1 208:19:2/6 
209:23:11/11 210:20:1/1 211:26:14/14 212:16:1/2 213:16:1/1 214:10:2/2 215:8:1/1 216:23:2/3 
217:22:6/6 218:23:2/2 219:19:4/4 220:15:6/11 221:8:5/5 222:7:5/5 223:34:24/24 224:14:7/20 
225:6:1/1 226:12:1/1 227:18:7/7 228:17:1/2 229:26:2/2 230:31:5/5 231:20:1/1 232:18:3/5 
233:9:1/1 234:23:1/1 235:29:16/16 236:20:3/4 237:12:6/6 238:19:3/3 239:12:2/2 240:20:5/6 
241:13:1/1 242:15:3/3 243:32:3/3 244:22:2/3 245:7:1/1 246:20:4/4 247:10:1/1 248:21:9/10 
249:22:1/1 250:21:1/1 251:33:2/2 252:14:1/2 253:11:1/1 254:28:4/4 255:14:1/1 256:26:2/9 
257:18:1/1 258:13:6/6 259:7:1/1 260:21:4/5 261:21:3/3 262:26:8/8 263:29:3/3 264:13:7/8 
265:17:2/2 266:22:2/2 267:17:1/1 268:15:7/22 269:8:1/1 270:24:10/10 271:6:1/1 272:19:9/10 
273:9:4/4 274:18:1/1 275:13:6/6 276:18:2/10 277:34:5/5 278:14:1/1 279:15:2/2 280:26:1/28 
281:7:1/1 282:34:3/3 283:17:8/8 284:16:1/2 285:24:5/5 286:20:2/2 287:23:7/7 288:18:8/33 
289:12:2/2 290:31:12/12 291:4:2/2 292:32:19/30 293:14:1/1 294:8:1/1 295:31:7/7 296:29:9/17 
297:10:1/1 298:7:1/1 299:12:1/1 300:25:2/27 301:19:1/1 302:10:1/1 303:10:1/1 304:18:10/36 
305:31:4/4 306:14:4/4 307:17:1/1 308:23:4/5 309:18:4/4 310:1:1/1 311:23:1/1 312:19:2/29 
313:36:8/8 314:16:1/1 315:19:7/7 316:12:2/3 317:16:2/2 318:12:1/1 319:20:1/1 320:15:6/34 
```
