//! `search` (SEI §8.4): the request is validated under the rules document —
//! canonical FEEN, canonical PMN, legality through `sashite-sanki-engine` —
//! and the history is replayed: whether the searched position is terminal,
//! and the bookkeeping the brain reads (the occurrences, the half-move
//! clock). The clock and the limits are kept as numbers for [`crate::think`],
//! which plans the time and runs the search.

use std::collections::{BTreeMap, HashMap};

use sashite_sanki_engine::domain::half_move::Move;
use sashite_sanki_engine::domain::outcome::Verdict;
use sashite_sanki_engine::engine;
use sashite_sanki_engine::pmn::{self, PmnError};
use sashite_sanki_engine::position::feen::FeenError;
use sashite_sanki_engine::position::{Position, PositionError};
use sashite_sanki_engine::terminal::{move_cap, move_limit, repetition};
use serde_json::{Map, Value};

use crate::protocol::{self, boolean, int, object, only, pointer, strings, Error, Request};

/// A validated search: the searched position, its legal moves (or the
/// `roots` the host restricted it to), whether it is terminal, the
/// bookkeeping the brain reads, and the time the host grants.
pub struct Searched {
    /// The position after `moves`, canonical.
    pub position: Position,
    /// The moves the engine may play: `roots` when given, else every legal move.
    pub candidates: Vec<Move>,
    /// Whether the searched position ends the game (`best` is then `null`).
    pub terminal: bool,
    /// Whether the search is infinite: neither `clock` nor `limits`.
    pub infinite: bool,
    /// Occurrences of every position reached, the searched one included
    /// (canonical FEEN → count): the repetition the brain must see.
    pub occurrences: BTreeMap<String, u32>,
    /// Half-moves since the last irreversible move, at the searched position.
    pub halfmove: u32,
    /// The clock, when the host sent one.
    pub clock: Option<Clock>,
    /// The limits, when the host sent them.
    pub limits: Limits,
}

/// The Active Player's clock (§8.4), in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// Time left before losing on time, or the host's cap — from receipt.
    pub deadline: u64,
    /// Budget left in the current period.
    pub remaining: u64,
    /// Time granted for each move of the current period.
    pub inc: u64,
    /// Moves remaining in the current period, when it has a quota.
    pub togo: Option<u64>,
    /// The host's latency outside the search.
    pub overhead: u64,
}

/// The caps (§8.4), each when given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// The iteration ceiling.
    pub depth: Option<u8>,
    /// The node budget.
    pub nodes: Option<u64>,
    /// The duration from receipt, in milliseconds.
    pub movetime: Option<u64>,
    /// Whether any cap was given.
    pub given: bool,
}

// ---- validation -------------------------------------------------------------

/// Validates a `search` request: `invalid` before `unsupported` before
/// `illegal`, one defect reported (§8.4).
pub fn validate(request: &Request) -> Result<Searched, Error> {
    let f = &request.fields;
    only(
        f,
        &[
            "rules", "position", "moves", "counters", "clock", "limits", "fresh", "roots",
        ],
        "",
    )?;

    // Shapes and domains first: every `invalid`.
    let rules = match f.get("rules") {
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(Error::invalid("`rules` must be a string", Some("/rules"))),
        None => return Err(Error::invalid("`rules` is required", Some("/rules"))),
    };
    let position_text = match f.get("position") {
        Some(Value::String(s)) => s.as_str(),
        Some(_) => {
            return Err(Error::invalid(
                "`position` must be a string",
                Some("/position"),
            ))
        }
        None => return Err(Error::invalid("`position` is required", Some("/position"))),
    };
    let moves = strings(f, "moves", "")?.unwrap_or_default();
    let counters = check_counters(f)?;
    let clock = match object(f, "clock", "")? {
        Some(clock) => Some(check_clock(clock)?),
        None => None,
    };
    let limits = match object(f, "limits", "")? {
        Some(limits) => check_limits(limits)?,
        None => Limits::default(),
    };
    // `fresh` is checked and ignored: the brain keeps nothing between two
    // searches (a new transposition table each time), so every search is
    // fresh (§8.4).
    boolean(f, "fresh", "")?;
    let roots = strings(f, "roots", "")?;
    if let Some(roots) = &roots {
        if roots.is_empty() {
            return Err(Error::invalid("`roots` must not be empty", Some("/roots")));
        }
        for (i, text) in roots.iter().enumerate() {
            if roots.get(..i).is_some_and(|earlier| earlier.contains(text)) {
                return Err(Error::invalid(
                    "a repeated Move",
                    Some(&pointer("roots", i)),
                ));
            }
        }
    }
    // The position-free half of the moves' validation, still `invalid`.
    let no_roots = Vec::new();
    for (key, list) in [
        ("moves", &moves),
        ("roots", roots.as_ref().unwrap_or(&no_roots)),
    ] {
        for (i, text) in list.iter().enumerate() {
            pmn::well_formed(text)
                .map_err(|e| Error::invalid(e.to_string(), Some(&pointer(key, i))))?;
        }
    }
    let parsed = Position::parse(position_text);
    if let Err(FeenError::Parse(e)) = &parsed {
        return Err(Error::invalid(
            format!("malformed FEEN: {e:?}"),
            Some("/position"),
        ));
    }

    // Then what the engine implements: `unsupported`.
    if rules != protocol::RULES {
        return Err(Error::unsupported(
            format!("rules `{rules}` are not implemented"),
            Some("/rules"),
        ));
    }
    let mut position = match parsed {
        Ok(p) => p,
        Err(FeenError::Position(PositionError::Style(_))) => {
            return Err(Error::unsupported(
                "a style outside W, J and C",
                Some("/position"),
            ))
        }
        // Then the position and the moves under the rules document: `illegal`.
        Err(FeenError::NotSankiBoard | FeenError::Position(PositionError::NotSankiBoard)) => {
            return Err(Error::illegal(
                "the board is not the 8×8 of Sanki",
                Some("/position"),
            ))
        }
        Err(FeenError::Parse(_)) => {
            return Err(Error::invalid("malformed FEEN", Some("/position")))
        }
    };
    if position.to_feen() != position_text {
        return Err(Error::illegal(
            "the FEEN is not the canonical form of the position",
            Some("/position"),
        ));
    }

    // The history: replay every move, keep the counters the endings read.
    let mut halfmove = counters.halfmove;
    let mut ply = counters.ply;
    let mut occurrences: HashMap<String, usize> = HashMap::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    bump(&mut occurrences, position_text.to_owned());
    count(&mut counts, position_text.to_owned());
    let mut terminal = is_terminal(&position, halfmove, ply, &occurrences);
    for (i, text) in moves.iter().enumerate() {
        let path = pointer("moves", i);
        if terminal {
            return Err(Error::illegal(
                "a Move played after a terminal position",
                Some(&path),
            ));
        }
        let mv = read_move(&position, text, &path)?;
        let applied = engine::apply_ply(&position, &mv)
            .map_err(|reason| Error::illegal(format!("illegal move: {reason:?}"), Some(&path)))?;
        halfmove = if applied.irreversible {
            0
        } else {
            halfmove.saturating_add(1)
        };
        ply = ply.saturating_add(1);
        position = applied.position;
        bump(&mut occurrences, position.to_feen());
        count(&mut counts, position.to_feen());
        terminal = is_terminal(&position, halfmove, ply, &occurrences);
    }

    // The candidates: `roots`, validated like moves, or every legal move.
    let candidates = match roots {
        Some(roots) => {
            if terminal {
                return Err(Error::illegal(
                    "no Move is legal in a terminal position",
                    Some(&pointer("roots", 0)),
                ));
            }
            roots
                .iter()
                .enumerate()
                .map(|(i, text)| read_move(&position, text, &pointer("roots", i)))
                .collect::<Result<Vec<_>, _>>()?
        }
        None => {
            if terminal {
                Vec::new()
            } else {
                engine::legal_moves(&position)
            }
        }
    };

    Ok(Searched {
        position,
        candidates,
        terminal,
        infinite: clock.is_none() && !limits.given,
        occurrences: counts,
        halfmove: u32::try_from(halfmove).unwrap_or(u32::MAX),
        clock,
        limits,
    })
}

/// A PMN string as a legal, canonical Move of `position`, with SEI's error
/// classes: a malformed string or a drop without its piece is `invalid`,
/// everything else `illegal`.
fn read_move(position: &Position, text: &str, path: &str) -> Result<Move, Error> {
    pmn::parse_canonical(position, text).map_err(|e| match e {
        PmnError::Malformed | PmnError::DropWithoutPiece => {
            Error::invalid(e.to_string(), Some(path))
        }
        other => Error::illegal(other.to_string(), Some(path)),
    })
}

/// The counters at `position`, each defaulted (§8.4).
struct Counters {
    halfmove: u64,
    ply: u64,
}

fn check_counters(f: &Map<String, Value>) -> Result<Counters, Error> {
    let Some(counters) = object(f, "counters", "")? else {
        return Ok(Counters {
            halfmove: 0,
            ply: 1,
        });
    };
    only(counters, &["halfmove", "ply"], "/counters")?;
    let halfmove = int(counters, "halfmove", 0, protocol::MAX_INT, "/counters")?.unwrap_or(0);
    let ply = int(counters, "ply", 1, protocol::MAX_INT, "/counters")?.unwrap_or(1);
    Ok(Counters {
        halfmove: u64::try_from(halfmove).unwrap_or(0),
        ply: u64::try_from(ply).unwrap_or(1),
    })
}

/// The clock's shapes and domains (§8.4), and the Active Player's numbers.
fn check_clock(clock: &Map<String, Value>) -> Result<Clock, Error> {
    only(clock, &["own", "opp", "overhead"], "/clock")?;
    let overhead = int(clock, "overhead", 0, protocol::MAX_INT, "/clock")?.unwrap_or(0);
    let Some(own) = object(clock, "own", "/clock")? else {
        return Err(Error::invalid("`own` is required", Some("/clock/own")));
    };
    check_side(own, "/clock/own", true)?;
    if let Some(opp) = object(clock, "opp", "/clock")? {
        check_side(opp, "/clock/opp", false)?;
    }
    let as_ms = |v: Option<i64>| v.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
    Ok(Clock {
        deadline: as_ms(int(own, "deadline", 0, protocol::MAX_INT, "/clock/own")?),
        remaining: as_ms(int(own, "remaining", 0, protocol::MAX_INT, "/clock/own")?),
        inc: as_ms(int(own, "inc", 0, protocol::MAX_INT, "/clock/own")?),
        togo: int(own, "togo", 1, protocol::MAX_INT, "/clock/own")?
            .and_then(|v| u64::try_from(v).ok()),
        overhead: as_ms(Some(overhead)),
    })
}

fn check_side(side: &Map<String, Value>, prefix: &str, own: bool) -> Result<(), Error> {
    let allowed: &[&str] = if own {
        &[
            "deadline",
            "remaining",
            "inc",
            "togo",
            "time",
            "carry",
            "next",
        ]
    } else {
        &["remaining", "inc", "togo", "time", "carry", "next"]
    };
    only(side, allowed, prefix)?;
    if own && side.get("deadline").is_none() {
        return Err(Error::invalid(
            "`deadline` is required",
            Some(&format!("{prefix}/deadline")),
        ));
    }
    int(side, "deadline", 0, protocol::MAX_INT, prefix)?;
    if int(side, "remaining", 0, protocol::MAX_INT, prefix)?.is_none() {
        return Err(Error::invalid(
            "`remaining` is required",
            Some(&format!("{prefix}/remaining")),
        ));
    }
    int(side, "inc", 0, protocol::MAX_INT, prefix)?;
    let togo = int(side, "togo", 1, protocol::MAX_INT, prefix)?;
    let time = int(side, "time", 0, protocol::MAX_INT, prefix)?;
    let carry = boolean(side, "carry", prefix)?;
    if togo.is_some() && time.is_none() {
        return Err(Error::invalid(
            "`time` is required with `togo`",
            Some(&format!("{prefix}/time")),
        ));
    }
    if carry.is_some() && togo.is_none() {
        return Err(Error::invalid(
            "`carry` requires `togo`",
            Some(&format!("{prefix}/carry")),
        ));
    }
    if let Some(next) = side.get("next") {
        let Value::Array(periods) = next else {
            return Err(Error::invalid(
                "`next` must be an array",
                Some(&format!("{prefix}/next")),
            ));
        };
        for (i, period) in periods.iter().enumerate() {
            let p = format!("{prefix}/next/{i}");
            let Value::Object(period) = period else {
                return Err(Error::invalid("a period must be an object", Some(&p)));
            };
            only(period, &["time", "inc", "moves", "carry"], &p)?;
            if int(period, "time", 0, protocol::MAX_INT, &p)?.is_none() {
                return Err(Error::invalid(
                    "`time` is required",
                    Some(&format!("{p}/time")),
                ));
            }
            int(period, "inc", 0, protocol::MAX_INT, &p)?;
            let moves = int(period, "moves", 1, protocol::MAX_INT, &p)?;
            if boolean(period, "carry", &p)?.is_some() && moves.is_none() {
                return Err(Error::invalid(
                    "`carry` requires `moves`",
                    Some(&format!("{p}/carry")),
                ));
            }
        }
    }
    Ok(())
}

/// The limits' shapes and domains (§8.4): non-empty, each in its domain.
/// `mate` is accepted and ignored: this engine proves no mate on its own,
/// the host combines it with another limit (§8.4, informative).
fn check_limits(limits: &Map<String, Value>) -> Result<Limits, Error> {
    only(limits, &["depth", "nodes", "movetime", "mate"], "/limits")?;
    if limits.is_empty() {
        return Err(Error::invalid(
            "`limits` must not be empty",
            Some("/limits"),
        ));
    }
    let depth = int(limits, "depth", 1, protocol::MAX_INT, "/limits")?;
    let nodes = int(limits, "nodes", 1, protocol::MAX_INT, "/limits")?;
    let movetime = int(limits, "movetime", 0, protocol::MAX_INT, "/limits")?;
    int(limits, "mate", 1, protocol::MAX_INT, "/limits")?;
    Ok(Limits {
        depth: depth.map(|d| u8::try_from(d).unwrap_or(u8::MAX)),
        nodes: nodes.and_then(|n| u64::try_from(n).ok()),
        movetime: movetime.and_then(|m| u64::try_from(m).ok()),
        given: true,
    })
}

/// Counts one more occurrence of a canonical position.
fn bump(occurrences: &mut HashMap<String, usize>, feen: String) {
    let count = occurrences.entry(feen).or_default();
    *count = count.saturating_add(1);
}

/// The same count, in the brain's shape.
fn count(counts: &mut BTreeMap<String, u32>, feen: String) {
    let count = counts.entry(feen).or_default();
    *count = count.saturating_add(1);
}

/// Whether a position ends the game under `sashite.sanki.kernel/1`: the
/// module's intrinsic statuses, or the endings that depend on history —
/// repetition, the move limit, the move cap (*SEI Rules Document — Sanki*
/// §Counters and terminal positions).
fn is_terminal(
    position: &Position,
    halfmove: u64,
    ply: u64,
    occurrences: &HashMap<String, usize>,
) -> bool {
    if !matches!(engine::status(position), Verdict::Ongoing) {
        return true;
    }
    let repeated =
        occurrences.get(&position.to_feen()).copied().unwrap_or(0) >= repetition::THREEFOLD;
    let limit = halfmove >= u64::from(move_limit::HALF_MOVE_LIMIT);
    let cap = ply > u64::from(move_cap::HALF_MOVE_CAP);
    repeated || limit || cap
}
