//! The search (SEI §8.4 *Stopping*, *Guaranteed result*): the time the host
//! grants is planned into one stop instant, the brain of
//! `sashite-sanki-player` runs under it — anytime, its depth-1 iteration
//! exempt from every stop, so that a legal move always comes out — and
//! the result is written as SEI's `info` and `done`.
//!
//! **Time.** A clock gives a hard bound, `own.deadline − overhead` from
//! receipt (§8.4 *Clock*), and a plan: the share of the current period this
//! move may take — `remaining / togo` on a period with a quota, a
//! twenty-fifth of `remaining` without one, the increment added either way,
//! and never less than a twenty-fifth of what the deadline counts beyond
//! `remaining` (the period about to end is not the end of the time) — of
//! which three quarters are spent. `movetime` is a cap in its own right. The search
//! stops at the earliest of the plan, the hard bound less a stopping
//! margin, the caps on depth and nodes, and the host's interruption.
//!
//! **Depth.** The `depth` option is the ceiling under a clock — the
//! strength of a bot, which its clock would otherwise decide — and of an
//! infinite search, which runs to it and then waits for the next request.
//! Under `limits` alone the caps bind on their own (§8.4 *Without a
//! clock*): the ceiling is then the engine's maximum, unless `limits.depth`
//! says less.
//!
//! **Scores.** `cp` is the brain's evaluation from the mover's viewpoint;
//! a mate score becomes `mate` (half-moves, signed). `wdl` is a
//! calibration of this engine's own: the win expectancy of a logistic on
//! `cp` with a scale of 400, and a draw share that fades with the
//! magnitude — hosts that decide on `wdl` get a monotone, bounded model,
//! not a measured one.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sashite_sanki_engine::engine;
use sashite_sanki_engine::pmn;
use sashite_sanki_engine::position::Position;
use sashite_sanki_player::{assess, choose, Choice, Context, Limits, Strength, MATE};
use serde_json::{json, Value};

use crate::protocol::{Event, Options, DEPTH_MAX};
use crate::search::Searched;

/// The least stopping margin, in milliseconds: what the search stops at,
/// before a bound, so that its last iteration is answered by it.
const STOP_MARGIN_MIN_MS: u64 = 30;

/// The most: the margin grows with the bound (a tenth), up to this.
const STOP_MARGIN_MAX_MS: u64 = 250;

/// The margin before a bound: a tenth of it, within the two limits above —
/// a scheduler's quantum on a loaded machine is what it covers, not a
/// share of the game.
const fn margin(bound_ms: u64) -> u64 {
    let tenth = bound_ms / 10;
    if tenth < STOP_MARGIN_MIN_MS {
        STOP_MARGIN_MIN_MS
    } else if tenth > STOP_MARGIN_MAX_MS {
        STOP_MARGIN_MAX_MS
    } else {
        tenth
    }
}

/// The range under `MATE` where a score is a mate: the brain's own window.
const MATE_RANGE: i32 = 1_000;

/// The share of the planned time this move spends: the rest is the
/// margin for the moves that follow, and the host's own latency.
const PLAN_SHARE_PER_MILLE: u64 = 750;

/// Without a quota, the fraction of the period one move may plan on.
const MOVES_TO_GO_WITHOUT_QUOTA: u64 = 25;

/// The brain's transposition table, in entries.
const TT_CAPACITY: usize = 200_000;

/// The plan of one search.
#[derive(Debug, Clone, Copy)]
pub struct Plan {
    /// When to stop, from receipt; `None` without a clock nor `movetime`.
    pub stop_at: Option<Instant>,
    /// The iteration ceiling.
    pub max_depth: u8,
    /// The node budget.
    pub max_nodes: Option<u64>,
    /// Whether the `done` waits for the next request.
    pub infinite: bool,
}

impl Plan {
    /// The safety net's plan: the depth-1 iteration alone, exempt from every
    /// stop, so that a legal move comes out before the search proper.
    #[must_use]
    pub const fn safety_net() -> Self {
        Self {
            stop_at: None,
            max_depth: 1,
            max_nodes: None,
            infinite: false,
        }
    }
}

/// Plans the time of a validated search received at `received`.
#[must_use]
pub fn plan(searched: &Searched, options: Options, received: Instant) -> Plan {
    let mut budget: Option<u64> = None;
    let mut cap = |ms: u64| budget = Some(budget.map_or(ms, |b| b.min(ms)));
    if let Some(clock) = searched.clock {
        // The hard bound, less the stopping margin.
        let hard = clock.deadline.saturating_sub(clock.overhead);
        cap(hard.saturating_sub(margin(hard)));
        // The plan: this move's share of the period, floored on the share
        // of what the deadline counts beyond it — the period about to end
        // is not the end of the time.
        let share = match clock.togo {
            Some(togo) => clock.remaining.checked_div(togo.max(1)).unwrap_or(0),
            None => clock
                .remaining
                .checked_div(MOVES_TO_GO_WITHOUT_QUOTA)
                .unwrap_or(0),
        }
        .saturating_add(clock.inc);
        let floor = hard
            .saturating_sub(clock.remaining)
            .checked_div(MOVES_TO_GO_WITHOUT_QUOTA)
            .unwrap_or(0);
        cap(share
            .max(floor)
            .saturating_mul(PLAN_SHARE_PER_MILLE)
            .checked_div(1000)
            .unwrap_or(0));
    }
    if let Some(movetime) = searched.limits.movetime {
        cap(movetime.saturating_sub(margin(movetime)));
    }
    // The ceiling: the option under a clock and for an infinite search, the
    // engine's maximum under limits alone; `limits.depth` caps it.
    let ceiling = if searched.clock.is_some() || searched.infinite {
        options.depth
    } else {
        DEPTH_MAX
    };
    let max_depth = searched
        .limits
        .depth
        .map_or(ceiling, |d| d.min(ceiling))
        .max(1);
    Plan {
        // An overflow of the instant (not reachable: it holds seconds in
        // sixty-four bits) stops at once rather than never.
        stop_at: budget.map(|ms| {
            received
                .checked_add(Duration::from_millis(ms))
                .unwrap_or(received)
        }),
        max_depth,
        max_nodes: searched.limits.nodes,
        infinite: searched.infinite,
    }
}

/// The searched position's context, in the brain's shape.
fn context<'a>(searched: &'a Searched, occurrences: &'a BTreeMap<String, u32>) -> Context<'a> {
    Context {
        position: &searched.position,
        occurrences,
        halfmove_clock: searched.halfmove,
    }
}

/// Runs the brain under the plan and the host's interruption, restricted to
/// the candidates (`roots`, or every legal move). `None` when no legal move
/// could be produced.
#[must_use]
pub fn run(
    searched: &Searched,
    seed: u64,
    plan: &Plan,
    cancelled: &Arc<AtomicBool>,
) -> Option<Choice> {
    let should_stop =
        || cancelled.load(Ordering::Relaxed) || plan.stop_at.is_some_and(|at| Instant::now() >= at);
    let limits = Limits {
        max_nodes: plan.max_nodes,
        should_stop: Some(&should_stop),
    };
    let strength = Strength {
        max_depth: plan.max_depth,
        tt_capacity: TT_CAPACITY,
        seed,
        ..Strength::default()
    };
    let occurrences = &searched.occurrences;
    let ctx = context(searched, occurrences);
    let restricted = searched.candidates.len() < engine::legal_moves(&searched.position).len();
    if !restricted {
        return choose(&ctx, &strength, &limits);
    }
    // `roots`: the replies searched one ply shallower, from the other
    // side, in rounds of increasing depth over every root — the best of the
    // negations of a complete round is the answer, so that the stop, which
    // falls in the middle of a round, never leaves a root unranked. The
    // depth-1 round is exempt from the stop, as the brain's own is. The
    // node budget is shared by all the searches.
    let child_depth = plan.max_depth.saturating_sub(1).max(1);
    let mut best: Option<Choice> = None;
    let mut nodes: u64 = 0;
    let exhausted = |nodes: u64| plan.max_nodes.is_some_and(|budget| nodes >= budget);
    'rounds: for depth in 1..=child_depth {
        let child_strength = Strength {
            max_depth: depth,
            ..strength
        };
        let mut round: Option<Choice> = None;
        for mv in &searched.candidates {
            if depth > 1 && (should_stop() || exhausted(nodes)) {
                break 'rounds;
            }
            let Ok(applied) = engine::apply_ply(&searched.position, mv) else {
                continue;
            };
            let mut child_occurrences = occurrences.clone();
            let feen = applied.position.to_feen();
            let count = child_occurrences.entry(feen).or_default();
            *count = count.saturating_add(1);
            let child = Context {
                position: &applied.position,
                occurrences: &child_occurrences,
                halfmove_clock: if applied.irreversible {
                    0
                } else {
                    searched.halfmove.saturating_add(1)
                },
            };
            let child_limits = Limits {
                max_nodes: plan
                    .max_nodes
                    .map(|budget| budget.saturating_sub(nodes).max(1)),
                ..limits
            };
            let (score, reached, pv, used) = match choose(&child, &child_strength, &child_limits) {
                Some(reply) => (reply.eval_cp, reply.depth, reply.pv, reply.nodes),
                // The reply's side has no legal move: its terminal score.
                None => (
                    assess(&child, &child_strength, &child_limits),
                    depth,
                    Vec::new(),
                    0,
                ),
            };
            nodes = nodes.saturating_add(used);
            if depth > 1 && reached < depth {
                // Interrupted: the round is incomplete, the previous stands.
                break 'rounds;
            }
            let eval_cp = from_reply(score);
            if round.as_ref().is_none_or(|b| eval_cp > b.eval_cp) {
                let mut line = Vec::with_capacity(pv.len().saturating_add(1));
                line.push(mv.clone());
                line.extend(pv);
                round = Some(Choice {
                    mv: mv.clone(),
                    eval_cp,
                    // The reply's depth plus this ply, within the cap: at a
                    // cap of one, the replies are still searched one ply.
                    depth: depth.saturating_add(1).min(plan.max_depth),
                    pv: line,
                    nodes: 0,
                });
            }
        }
        best = round.or(best);
        // A mate found is final: no deeper round changes it.
        if best
            .as_ref()
            .is_some_and(|b| b.eval_cp >= MATE.saturating_sub(MATE_RANGE))
        {
            break;
        }
    }
    let mut best = best?;
    best.nodes = nodes;
    Some(best)
}

/// A reply's score as the mover's: negated, a mate one half-move farther
/// — the reply counts its plies from its own root.
fn from_reply(score: i32) -> i32 {
    let shifted = if score >= MATE.saturating_sub(MATE_RANGE) {
        score.saturating_sub(1)
    } else if score <= MATE.saturating_neg().saturating_add(MATE_RANGE) {
        score.saturating_add(1)
    } else {
        score
    };
    shifted.saturating_neg()
}

/// The brain's tie-breaking seed: the option, or one drawn from the clock
/// and the process for `0`.
#[must_use]
pub fn seed_of(seed: u32) -> u64 {
    if seed != 0 {
        return u64::from(seed);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (nanos as u64) ^ (u64::from(std::process::id()).rotate_left(32)) | 1
}

/// The variations of a choice, in canonical PMN: the principal variation as
/// far as it stays legal from the searched position, and its score.
#[must_use]
pub fn variations(position: &Position, choice: &Choice) -> Value {
    let mut pv = Vec::with_capacity(choice.pv.len());
    let mut current = position.clone();
    for mv in &choice.pv {
        let Ok(text) = pmn::to_pmn(&current, mv) else {
            break;
        };
        let Ok(applied) = engine::apply_ply(&current, mv) else {
            break;
        };
        pv.push(text);
        current = applied.position;
    }
    json!([{ "pv": pv, "score": score(choice.eval_cp) }])
}

/// A brain score as SEI writes it: `mate` in signed half-moves at the mate
/// range, `cp` otherwise, `wdl` always.
#[must_use]
pub fn score(eval_cp: i32) -> Value {
    if eval_cp >= MATE.saturating_sub(MATE_RANGE) {
        let plies = MATE.saturating_sub(eval_cp);
        return json!({ "mate": plies, "wdl": [1000, 0, 0] });
    }
    if eval_cp <= MATE.saturating_neg().saturating_add(MATE_RANGE) {
        let plies = MATE.saturating_add(eval_cp);
        return json!({ "mate": plies.saturating_neg(), "wdl": [0, 0, 1000] });
    }
    json!({ "cp": eval_cp, "wdl": wdl(eval_cp) })
}

/// The win-draw-loss calibration of this engine, per mille.
#[must_use]
pub fn wdl(eval_cp: i32) -> [u32; 3] {
    let x = f64::from(eval_cp) / 400.0;
    let win_expectancy = 1.0 / (1.0 + 10f64.powf(-x));
    // The draw share: two fifths at equality, fading with the magnitude.
    let draw = 0.4 * (-(x * x)).exp();
    let per_mille = |v: f64| {
        let scaled = (v * 1000.0).round();
        if scaled.is_finite() && scaled >= 0.0 {
            // Bounded by 1000 by construction; the clamp is for the rounding.
            (scaled as u32).min(1000)
        } else {
            0
        }
    };
    // Win and loss rounded alike, so that the model is antisymmetric; the
    // draw takes the remainder.
    let w = per_mille((1.0 - draw) * win_expectancy);
    let l = per_mille((1.0 - draw) * (1.0 - win_expectancy)).min(1000u32.saturating_sub(w));
    [w, 1000u32.saturating_sub(w).saturating_sub(l), l]
}

/// The `info` of the safety net: the first legal move the brain holds.
#[must_use]
pub fn info(id: i64, position: &Position, choice: &Choice, elapsed: Duration) -> Event {
    Event::from_value(json!({
        "re": id,
        "ev": "info",
        "depth": choice.depth,
        "nodes": choice.nodes,
        "elapsed": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        "variations": variations(position, choice),
    }))
}

/// The `done` of a search: `best` in canonical PMN, the variation, the
/// state of the search at its end.
#[must_use]
pub fn done(id: i64, position: &Position, choice: &Choice, elapsed: Duration) -> Option<Event> {
    let best = pmn::to_pmn(position, &choice.mv).ok()?;
    Some(Event::from_value(json!({
        "re": id,
        "ev": "done",
        "best": best,
        "depth": choice.depth,
        "nodes": choice.nodes,
        "elapsed": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        "variations": variations(position, choice),
    })))
}

/// The `done` of a terminal position.
#[must_use]
pub fn done_terminal(id: i64) -> Event {
    Event::from_value(json!({ "re": id, "ev": "done", "best": null }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn mate_scores_are_written_in_half_moves() {
        assert_eq!(score(MATE - 1), json!({ "mate": 1, "wdl": [1000, 0, 0] }));
        assert_eq!(score(MATE - 5), json!({ "mate": 5, "wdl": [1000, 0, 0] }));
        assert_eq!(
            score(-(MATE - 2)),
            json!({ "mate": -2, "wdl": [0, 0, 1000] })
        );
        assert_eq!(score(0)["cp"], 0);
        assert_eq!(score(MATE - 1_001)["cp"], MATE - 1_001);
    }

    #[test]
    fn a_reply_score_is_negated_and_its_mate_one_ply_farther() {
        assert_eq!(
            from_reply(-(MATE - 2)),
            MATE - 3,
            "the reply is mated in 2: we mate in 3"
        );
        assert_eq!(
            from_reply(MATE - 1),
            -(MATE - 2),
            "the reply mates in 1: we are mated in 2"
        );
        assert_eq!(
            from_reply(-MATE),
            MATE - 1,
            "a reply without a move: mate in 1"
        );
        assert_eq!(from_reply(42), -42);
        assert_eq!(score(from_reply(-MATE))["mate"], 1);
    }

    #[test]
    fn wdl_is_a_calibration_that_sums_to_a_thousand() {
        let even = wdl(0);
        assert_eq!(even.iter().sum::<u32>(), 1000);
        assert_eq!(even[0], even[2], "symmetric at equality");
        assert_eq!(even[1], 400, "two fifths of draws at equality");
        let mut last = 0;
        for cp in (-3_000..=3_000).step_by(50) {
            let [w, d, l] = wdl(cp);
            assert_eq!(w + d + l, 1000, "{cp}");
            assert!(w >= last, "monotone at {cp}");
            last = w;
            let [lw, _, ll] = wdl(-cp);
            assert_eq!((lw, ll), (l, w), "antisymmetric at {cp}");
        }
        assert!(wdl(2_000)[0] >= 990);
        assert!(wdl(2_000)[1] <= 5);
    }

    #[test]
    fn the_plan_takes_the_earliest_bound() {
        use crate::search::{Clock, Limits};
        use sashite_sanki_engine::domain::variant::Variant;
        use sashite_sanki_engine::rules;
        let Ok(position) = rules::initial_position(Variant::Chess, Variant::Chess) else {
            unreachable!("the initial position of chess exists");
        };
        let mut searched = Searched {
            candidates: engine::legal_moves(&position),
            position,
            terminal: false,
            infinite: false,
            occurrences: BTreeMap::new(),
            halfmove: 0,
            clock: Some(Clock {
                deadline: 2_150,
                remaining: 2_000,
                inc: 100,
                togo: Some(10),
                overhead: 50,
            }),
            limits: Limits::default(),
        };
        let received = Instant::now();
        let options = Options { depth: 6, seed: 0 };
        // The plan: (2000 / 10 + 100) × 0.75 = 225 ms, before the hard bound.
        let planned = plan(&searched, options, received);
        assert_eq!(planned.stop_at, Some(received + Duration::from_millis(225)));
        assert_eq!(planned.max_depth, 6);
        // The hard bound wins when the period is generous: 300 ms of
        // deadline less 100 of overhead, less the least margin.
        searched.clock = Some(Clock {
            deadline: 300,
            remaining: 60_000,
            inc: 0,
            togo: None,
            overhead: 100,
        });
        let planned = plan(&searched, options, received);
        assert_eq!(planned.stop_at, Some(received + Duration::from_millis(170)));
        // A period spent while the deadline stands (the bank exhausted, the
        // overtime counted in the deadline): a twenty-fifth of what lies
        // beyond the period, three quarters of it — not nothing.
        searched.clock = Some(Clock {
            deadline: 3_050,
            remaining: 0,
            inc: 0,
            togo: None,
            overhead: 50,
        });
        let planned = plan(&searched, options, received);
        assert_eq!(planned.stop_at, Some(received + Duration::from_millis(90)));
        // A long bound: the margin is a tenth of it, at most 250 ms.
        searched.clock = Some(Clock {
            deadline: 7_000,
            remaining: 0,
            inc: 10_000,
            togo: Some(1),
            overhead: 0,
        });
        let planned = plan(&searched, options, received);
        assert_eq!(
            planned.stop_at,
            Some(received + Duration::from_millis(6_750))
        );
        // A deadline within the overhead: stop at once, answer the net.
        searched.clock = Some(Clock {
            deadline: 40,
            remaining: 0,
            inc: 0,
            togo: None,
            overhead: 50,
        });
        let planned = plan(&searched, options, received);
        assert_eq!(planned.stop_at, Some(received));
        // `movetime` caps, and `limits.depth` too — under a clock, never
        // above the option.
        searched.clock = Some(Clock {
            deadline: 60_000,
            remaining: 60_000,
            inc: 0,
            togo: None,
            overhead: 0,
        });
        searched.limits = Limits {
            depth: Some(9),
            nodes: Some(1_000),
            movetime: Some(100),
            given: true,
        };
        let planned = plan(&searched, options, received);
        assert_eq!(planned.stop_at, Some(received + Duration::from_millis(70)));
        assert_eq!((planned.max_depth, planned.max_nodes), (6, Some(1_000)));
        // Limits without a clock bind on their own: the ceiling is the
        // engine's maximum, or `limits.depth`.
        searched.clock = None;
        let planned = plan(&searched, options, received);
        assert_eq!(planned.max_depth, 9);
        searched.limits = Limits {
            movetime: Some(2_000),
            given: true,
            ..Limits::default()
        };
        let planned = plan(&searched, options, received);
        assert_eq!(planned.max_depth, DEPTH_MAX);
        assert_eq!(
            planned.stop_at,
            Some(received + Duration::from_millis(1_800))
        );
        // Neither clock nor limits: no stop instant, the option's ceiling.
        searched.limits = Limits::default();
        searched.infinite = true;
        let planned = plan(&searched, options, received);
        assert_eq!(planned.stop_at, None);
        assert_eq!(planned.max_depth, 6);
        assert!(planned.infinite);
    }
}
