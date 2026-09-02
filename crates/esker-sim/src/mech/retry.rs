//! A client never gives up while it is making progress, and never loops without it.
//!
//! The mechanism is `9791e16`. `Backend::begin` came back as `gave up after 9 attempts: region
//! epoch does not match` on a saturated box, and the number beside it is the whole argument: it
//! had spent 2.266 s of a 10 s deadline. The client did not run out of time, it ran out of
//! attempts, with 77% of its own budget for the call unspent.
//!
//! Every one of those nine refusals was **progress**. An `EpochNotMatch` carries the regions that
//! replaced the one asked about, so each attempt leaves the cache more correct than it found it
//! and the next is aimed better. Counting those against the same budget as a store that will not
//! answer treats "you learned something, try again" as "this is not working". So the budget counts
//! *consecutive* attempts that taught the client nothing, and the deadline is what stops a region
//! whose epoch never settles.
//!
//! # What this model explores that the fix's tests do not
//!
//! The fix landed with two scripts: ten refusals each teaching a newer epoch, and two hundred of
//! them. Both are runs of a single kind of answer. What a saturated cluster actually produces is a
//! **mixture** — some refusals that teach something, some that teach nothing, in an order nobody
//! chooses — and the two rules interact only in a mixture. A budget that resets on progress and a
//! budget that never resets agree on every uniform script; they disagree on
//! `fruitless × 5, progress, fruitless × 5`, which the model draws and the hand-written tests do
//! not contain.
//!
//! # The ground truth is arithmetic over the script
//!
//! The model wrote the script, so it knows for every answer whether it taught anything. It walks
//! its own script to decide what the client was obliged to do, and compares. It never asks the
//! client what it thought it had learned.

use esker_base::rng::Pcg32;

/// What a store gives back for one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// A refusal carrying **strictly newer** routing than the attempt was addressed with.
    ///
    /// Progress: the cache is more correct than it was, and the next attempt is aimed better.
    Progress,
    /// A refusal that teaches this client nothing — a leader hint around a region that is not
    /// moving. Chasing one is exactly the loop the budget exists to stop.
    Fruitless,
    /// The call is answered.
    Answered,
}

/// A scripted sequence of answers, and what the store keeps saying once it runs out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// The answers, in order, one per attempt.
    pub answers: Vec<Answer>,
    /// What every attempt past `answers` gets.
    pub tail: Answer,
}

/// The two numbers the client's own loop is bounded by, as the binding reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Consecutive fruitless attempts allowed before the client gives up. The client stops on
    /// the attempt *after* this many, so `max_fruitless + 1` calls are made.
    pub max_fruitless: u32,
    /// The call deadline, in milliseconds.
    pub deadline_ms: u64,
}

/// What the client did with a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The call was answered.
    Answered {
        /// Calls made, the first included.
        calls: u32,
    },
    /// The client stopped because it ran out of attempts.
    OutOfAttempts {
        /// Calls made.
        calls: u32,
        /// Simulated milliseconds spent.
        elapsed_ms: u64,
    },
    /// The client stopped because it ran out of time.
    OutOfTime {
        /// Calls made.
        calls: u32,
        /// Simulated milliseconds spent.
        elapsed_ms: u64,
    },
    /// Anything else, which is a failure of the binding rather than of the client.
    Other {
        /// Calls made.
        calls: u32,
        /// What came back.
        what: String,
    },
}

impl Verdict {
    /// Calls made, however it ended.
    #[must_use]
    pub fn calls(&self) -> u32 {
        match self {
            Self::Answered { calls }
            | Self::OutOfAttempts { calls, .. }
            | Self::OutOfTime { calls, .. }
            | Self::Other { calls, .. } => *calls,
        }
    }
}

/// The decision under test: run one call against a script and say what happened.
///
/// `crates/esker-client/tests/sim_retry.rs` implements this with the real `RawClient` over
/// `FakeTransport` and `FakeClock`. That binding is the only thing here that proves anything about
/// the client.
pub trait RetryClient {
    /// The client's own limits, so the model's arithmetic uses the real numbers.
    fn budget(&self) -> Budget;
    /// One call against `script`, with no wall clock and no socket.
    fn call(&self, script: &Script) -> Verdict;
}

/// What the model says the client was obliged to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Obliged {
    /// The script answers on this call, and nothing before it entitled the client to stop.
    Answer {
        /// The call the answer arrives on, counting from one.
        on_call: u32,
    },
    /// The script stalls: this many fruitless answers in a row, ending on this call.
    GiveUpOnAttempts {
        /// The call the client must stop on, counting from one.
        on_call: u32,
    },
    /// The script never answers and never stalls, so only the deadline can stop it.
    RunOutOfTime,
}

/// A violation of one of the two halves, with the seed to reproduce it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// The client gave up on attempts while the script was still teaching it something.
    ///
    /// The pre-`9791e16` failure, and the one that costs a caller a call that was going to work.
    GaveUpWhileMakingProgress {
        /// The run's seed.
        seed: u64,
        /// What the script obliged.
        obliged: Obliged,
        /// What happened.
        verdict: Verdict,
        /// The script, rendered.
        script: String,
        /// Simulated milliseconds spent when it gave up.
        elapsed_ms: u64,
        /// The deadline it had.
        deadline_ms: u64,
    },
    /// The client kept calling a store that was teaching it nothing.
    LoopedWithoutProgress {
        /// The run's seed.
        seed: u64,
        /// What the script obliged.
        obliged: Obliged,
        /// What happened.
        verdict: Verdict,
        /// The script, rendered.
        script: String,
    },
    /// The client stopped on the wrong call, or in the wrong way.
    WrongEnding {
        /// The run's seed.
        seed: u64,
        /// What the script obliged.
        obliged: Obliged,
        /// What happened.
        verdict: Verdict,
        /// The script, rendered.
        script: String,
    },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GaveUpWhileMakingProgress {
                seed,
                obliged,
                verdict,
                script,
                elapsed_ms,
                deadline_ms,
            } => write!(
                formatter,
                "seed {seed}: the client gave up on attempts having spent {elapsed_ms}ms of a \
                 {deadline_ms}ms deadline. The script was [{script}], which obliged \
                 {obliged:?}; it answered {verdict:?}. A refusal that teaches a newer epoch is \
                 progress, and progress does not spend the budget (9791e16)"
            ),
            Self::LoopedWithoutProgress {
                seed,
                obliged,
                verdict,
                script,
            } => write!(
                formatter,
                "seed {seed}: the client kept calling a store that taught it nothing. The script \
                 was [{script}], which obliged {obliged:?}; it answered {verdict:?}. \
                 \"Progress does not spend the budget\" is only safe while something else counts"
            ),
            Self::WrongEnding {
                seed,
                obliged,
                verdict,
                script,
            } => write!(
                formatter,
                "seed {seed}: the script [{script}] obliged {obliged:?} and the client answered \
                 {verdict:?}"
            ),
        }
    }
}

impl std::error::Error for Violation {}

/// How many answers a drawn script holds.
///
/// Ten, and the ceiling is not arbitrary: the backoff schedule is 10 ms doubling to a 2 s cap, so
/// nine waits cost at most 4,550 ms against a 10 s deadline. A longer script would let the
/// deadline end a run the model meant to end on attempts, and the checker would have to accept
/// two answers where it should accept one.
const SCRIPT_LEN: usize = 10;

impl Script {
    /// A mixture of answers drawn from `seed`.
    ///
    /// Weighted toward refusals — a script that answers on its second call exercises nothing —
    /// and every fifth script is a deliberate **stall**: a long run of fruitless answers, which is
    /// the half of the rule that has to keep working.
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        let mut rng = Pcg32::new(seed, 0x7e77);
        if rng.below(5) == 0 {
            // A store that will not answer, reached after a few refusals that did teach
            // something. The budget must still stop this.
            let lead = rng.below(3) as usize;
            let mut answers = vec![Answer::Progress; lead];
            answers.resize(SCRIPT_LEN + lead, Answer::Fruitless);
            return Self {
                answers,
                tail: Answer::Fruitless,
            };
        }
        let answers = (0..SCRIPT_LEN)
            .map(|_| {
                if rng.below(4) == 0 {
                    Answer::Fruitless
                } else {
                    Answer::Progress
                }
            })
            .collect();
        Self {
            answers,
            // The store answers once the script runs out, so a run that never stalls is obliged
            // to reach it. That is the shape `9791e16` is about: ten refusals, then success.
            tail: Answer::Answered,
        }
    }

    /// A region whose epoch never settles: every attempt learns something and none succeeds.
    ///
    /// The other half of the fix — "progress does not spend the budget" is only safe while
    /// something else counts, so this must end at the **deadline**.
    #[must_use]
    pub fn endless_progress() -> Self {
        Self {
            answers: Vec::new(),
            tail: Answer::Progress,
        }
    }

    /// The answer for attempt `index`, counting from zero.
    #[must_use]
    pub fn answer(&self, index: usize) -> Answer {
        self.answers.get(index).copied().unwrap_or(self.tail)
    }

    /// What the client was obliged to do, walked from the script the model itself wrote.
    ///
    /// This is the ground truth, and it is arithmetic rather than an opinion: the model knows
    /// which answers taught something because it decided which ones would.
    #[must_use]
    pub fn obliges(&self, budget: Budget) -> Obliged {
        let mut fruitless = 0_u32;
        // One past the scripted answers, so a `tail` that answers or stalls is walked too. A
        // `tail` of `Progress` falls out of the loop, which is `RunOutOfTime`.
        for index in 0..=self.answers.len() {
            match self.answer(index) {
                Answer::Answered => {
                    return Obliged::Answer {
                        on_call: u32::try_from(index).unwrap_or(u32::MAX) + 1,
                    };
                }
                Answer::Progress => fruitless = 0,
                Answer::Fruitless => {
                    fruitless += 1;
                    if fruitless > budget.max_fruitless {
                        return Obliged::GiveUpOnAttempts {
                            on_call: u32::try_from(index).unwrap_or(u32::MAX) + 1,
                        };
                    }
                }
            }
        }
        Obliged::RunOutOfTime
    }

    /// The script as a short string, for a failure message.
    #[must_use]
    pub fn rendered(&self) -> String {
        let mut out: String = self
            .answers
            .iter()
            .map(|answer| match answer {
                Answer::Progress => 'p',
                Answer::Fruitless => 'f',
                Answer::Answered => 'a',
            })
            .collect();
        out.push_str(match self.tail {
            Answer::Progress => " then p forever",
            Answer::Fruitless => " then f forever",
            Answer::Answered => " then a",
        });
        out
    }
}

/// Checks one call against what its script obliged.
///
/// # Errors
///
/// One of the three [`Violation`]s, named so that the failure says which half of the rule broke.
pub fn check(
    seed: u64,
    script: &Script,
    budget: Budget,
    verdict: &Verdict,
) -> Result<(), Violation> {
    let obliged = script.obliges(budget);
    let rendered = script.rendered();
    match (obliged, verdict) {
        (Obliged::Answer { on_call }, Verdict::Answered { calls }) if *calls == on_call => Ok(()),

        // The finding. The script was still teaching this client something and it stopped anyway,
        // handing the caller a failure for a call that had seconds left to succeed in.
        (
            Obliged::Answer { .. } | Obliged::RunOutOfTime,
            Verdict::OutOfAttempts { elapsed_ms, .. },
        ) => Err(Violation::GaveUpWhileMakingProgress {
            seed,
            obliged,
            verdict: verdict.clone(),
            script: rendered,
            elapsed_ms: *elapsed_ms,
            deadline_ms: budget.deadline_ms,
        }),

        (Obliged::GiveUpOnAttempts { on_call }, Verdict::OutOfAttempts { calls, .. })
            if *calls == on_call =>
        {
            Ok(())
        }
        // A stalled script that the client kept calling: either it answered something else, or it
        // rode the deadline instead of the budget. Both are the loop the budget exists to stop.
        (Obliged::GiveUpOnAttempts { .. }, _) => Err(Violation::LoopedWithoutProgress {
            seed,
            obliged,
            verdict: verdict.clone(),
            script: rendered,
        }),

        // Only the deadline can end an endless run of progress, and it must have actually spent
        // the time — an immediate `DeadlineExceeded` would be the budget wearing another name.
        (Obliged::RunOutOfTime, Verdict::OutOfTime { elapsed_ms, .. })
            if *elapsed_ms * 2 >= budget.deadline_ms =>
        {
            Ok(())
        }

        _ => Err(Violation::WrongEnding {
            seed,
            obliged,
            verdict: verdict.clone(),
            script: rendered,
        }),
    }
}

/// What a clean run did, so a test can say the model reached the shapes it claims to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Scripts run.
    pub scripts: u64,
    /// Scripts that obliged an answer.
    pub obliged_answer: u64,
    /// Of those, the ones that took more calls than the budget would have allowed if progress
    /// had counted against it. **The pre-fix failures**, and a run with none of them is a run
    /// that proves nothing.
    pub answered_past_the_old_budget: u64,
    /// Scripts that obliged the client to give up on attempts.
    pub obliged_give_up: u64,
    /// Scripts that mixed both kinds of refusal — the shape neither hand-written test contains.
    pub mixed: u64,
}

/// Runs `seeds` scripts against `client`, checking each.
///
/// # Errors
///
/// The first [`Violation`] any script produces.
pub fn run<C: RetryClient>(seeds: &[u64], client: &C) -> Result<Report, Violation> {
    let budget = client.budget();
    let mut report = Report::default();
    for &seed in seeds {
        let script = Script::seeded(seed);
        let verdict = client.call(&script);
        check(seed, &script, budget, &verdict)?;

        report.scripts += 1;
        let progress = script
            .answers
            .iter()
            .filter(|answer| **answer == Answer::Progress)
            .count();
        let fruitless = script
            .answers
            .iter()
            .filter(|answer| **answer == Answer::Fruitless)
            .count();
        if progress > 0 && fruitless > 0 {
            report.mixed += 1;
        }
        match script.obliges(budget) {
            Obliged::Answer { on_call } => {
                report.obliged_answer += 1;
                if on_call > budget.max_fruitless + 1 {
                    report.answered_past_the_old_budget += 1;
                }
            }
            Obliged::GiveUpOnAttempts { .. } => report.obliged_give_up += 1,
            Obliged::RunOutOfTime => {}
        }
    }
    Ok(report)
}

/// Runs the endless-progress script, which only the deadline can stop.
///
/// # Errors
///
/// A [`Violation`] if the client stopped on attempts, or stopped without spending the time.
pub fn run_endless_progress<C: RetryClient>(client: &C) -> Result<Verdict, Violation> {
    let budget = client.budget();
    let script = Script::endless_progress();
    let verdict = client.call(&script);
    check(0, &script, budget, &verdict)?;
    Ok(verdict)
}
