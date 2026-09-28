# sanki-sei-player

An engine for the **Sashité Engine Interface** — [SEI 1.0.0](https://sashite.dev/specs/sei/1.0.0/) —
that plays [Sanki](https://sashite.com/) (chess, ōgi and xiongqi on one 8×8
board, in any pairing) with the reference search of
[`sashite-sanki-player`](https://github.com/sashite/sanki-player.rs): an
iterative-deepening alpha-beta with a transposition table, quiescence, and
the evaluation of ADR-0015 — under SEI's clock.

It is the **sparring partner**: the engine a bot runs when it should play a
game rather than a random move, and the engine to compare a stronger one
against. Give it to [`sanki-bot`](https://github.com/sashite/sanki-bot.rs):

```sh
cargo install sashite-sanki-sei-player   # the binary: sanki-sei-player
sanki-bot --engine sanki-sei-player
```

## What it is made of

| Concept | Where |
|---|---|
| The engine protocol | [SEI](https://sashite.dev/specs/sei/1.0.0/) — how a host and an engine talk |
| What SEI means for Sanki | *SEI Rules Document — Sanki*: canonical FEEN and PMN, styles `W` `J` `C`, the nine pairings |
| The rules | [`sashite-sanki-engine`](https://github.com/sashite/sanki-engine.rs): legal moves, `apply`, terminal statuses, and `pmn`, the PMN converters |
| The brain | [`sashite-sanki-player`](https://github.com/sashite/sanki-player.rs): `choose`, anytime, pure — it never reads a clock |
| The protocol side | forked from [`sanki-sei-random-engine`](https://github.com/sashite/sanki-sei-random-engine.rs): the transport, the envelope, the validation of every request, and its tests |
| The bot that hosts an engine on Nostr | [`sanki-bot.rs`](https://github.com/sashite/sanki-bot.rs): challenges, sessions, Plies, clocks, Conclusions |
| **This engine** | the clock, planned; the search, on a thread; the scores, in SEI's shape |

## Try it

```sh
cargo run --release
```

Then type, one request per line (the engine answers one event per line):

```
{"id":1,"op":"hello","versions":[1]}
{"id":2,"op":"configure","options":{"depth":4,"seed":7}}
{"id":3,"op":"search","rules":"sashite.sanki.kernel/1","position":"-rnbik^bn-r/+f+f+f+f+f+f+f+f/8/8/8/8/+P+P+P+P+P+P+P+P/-RNBQK^BN-R / W/j","moves":["e2-e4"],"clock":{"own":{"deadline":5000,"remaining":60000},"overhead":300}}
```

```
{"engine":{"name":"sanki-sei-player",…},"ev":"done","re":1,"rules":{"sashite.sanki.kernel/1":{}},"version":1,…}
{"ev":"done","re":2}
{"depth":1,"elapsed":2,"ev":"info","nodes":45,"re":3,"variations":[{"pv":["c7-c5"],"score":{"cp":-156,"wdl":[190,344,466]}}]}
{"best":"e7-e5","depth":4,"elapsed":842,"ev":"done","nodes":18144,"re":3,"variations":[{"pv":["e7-e5","d1-f3","g8-f6","d2-d4"],"score":{"cp":35,"wdl":[332,397,271]}}]}
```

Close the input (Ctrl-D) and the engine exits with code 0.

## Options

| Option | Default | Domain | Meaning |
|---|---|---|---|
| `depth` | 4 | 1 … 12 | The iterative-deepening ceiling **under a clock** — a bot's strength, which its clock would otherwise decide — and of an infinite search. Under `limits` alone (`movetime`, `nodes`, `depth`, no clock) the caps bind on their own, as SEI asks: the ceiling is then 12, or `limits.depth`. |
| `seed` | 0 | 0 … 2³¹ − 1 | Breaks the ties among equal-best root moves. `0` draws a seed from the clock at every search: two bots left unconfigured do not play the same game twice. A configured seed, with a cap on the depth and no clock, replays the same game. |

The strength is the depth: 4 answers in a fraction of a second on the
opening and longer in a full middlegame; the clock decides what the
depth cannot.

## Time

SEI hands the engine a clock; the brain never reads one. Between the two,
this engine plans a **stop instant** from the receipt of the `search`:

- the **hard bound** is `own.deadline − overhead`, less a stopping margin —
  a tenth of the bound, between 30 and 250 ms, what a loaded machine's
  scheduling costs — and the `done` is emitted by then, whatever the plan
  (§8.4);
- the **plan** is this move's share of the period: `remaining / togo` on a
  period with a quota, a twenty-fifth of `remaining` without one, the
  increment added either way, and never less than a twenty-fifth of what
  the deadline counts beyond `remaining` (a bank about to run out is not
  the end of the time when an overtime follows) — of which three quarters
  are spent;
- `movetime` is a cap in its own right; `depth` and `nodes` are hard caps.

The search stops at the earliest of these and answers with the last
completed iteration. The **depth-1 iteration is exempt from every stop**
(`sashite-sanki-player`'s rule): it is what guarantees a legal move whatever
the clock, and it is emitted first, as the safety-net `info`.

Without `clock` nor `limits`, the search is infinite: it runs to the
configured depth and keeps its `done` until the next request (§7 rule 2).
`ping` is answered at its receipt while a search runs; `cancel`, or any
other request, stops it within the bound of §8.4 — the brain polls the
interruption at every node.

## Scores

`cp` is the brain's evaluation from the mover's viewpoint. A mate found is
written as `mate`, in signed half-moves. `wdl` is a calibration of this
engine's own — the win expectancy of a logistic on `cp` with a scale of
400, and a draw share of two fifths at equality that fades with the
magnitude; a mate is `[1000, 0, 0]` or `[0, 0, 1000]`. It is monotone,
bounded and antisymmetric, not measured: a host that resigns or offers
draws on `wdl` (as `sanki-bot`'s `[play.resign]` and `[play.offer_draw]`
do) gets thresholds that mean what they say.

## What this engine does not do

- **Prove a mate on request.** `limits.mate` is accepted and ignored: a host
  combines it with another limit, as SEI advises.
- **Keep tables between searches.** Each `search` starts a fresh
  transposition table, so `fresh: true` changes nothing: every search
  already answers as a fresh process would.
- **Judge a position beyond its form.** A FEEN is refused when malformed
  (`invalid`), outside the 8×8 board (`illegal`), with a style outside
  `W`/`J`/`C` (`unsupported`), or not in the canonical form the rules
  document fixes (`illegal`). A position that is canonical in form but
  unreachable — two royals in check at once, say — is played as it stands.

## Errors, as SEI classes them

| The host sent | The engine answers |
|---|---|
| a bad envelope: an `id` that does not increase, a duplicate key, a fraction, a request before `hello`, a second `hello` | `{"ev":"error","code":"invalid"}` without `re`, then exit code 1 |
| an unknown field, a value out of domain, a drop without its piece (`*e4`) | `invalid`, with the field's JSON Pointer |
| rules other than `sashite.sanki.kernel/1`, a style outside Sanki | `unsupported` |
| an illegal move, a move spelled in a non-canonical way (`e1-g1` for a castling), a move after the end of the game | `illegal`, with the pointer of the move |
| a non-canonical FEEN, a board that is not 8×8 | `illegal`, with `/position` |

When a request has several defects, one is reported, in SEI's order:
`invalid` before `unsupported` before `illegal`.

`roots` restricts the search to the moves the host names: their replies are
searched one ply shallower from the other side, in rounds of increasing
depth over every root, and the best of the negations of the last complete
round is played — the stop never leaves a root unranked — its mate distance
counted from this side.

## Tests

`cargo test` drives the binary as a host does: the opening, the error
classes, the fatal envelope errors and exit codes, the safety-net `info`,
the infinite search with `ping` and `cancel`, `roots`, the deadline less
the overhead, the plan, `movetime`, `nodes`, the bounded stop, a seeded
replay, mates in one and in three scored as `mate` (with and without
`roots`), and games at depth 2 on the nine pairings with every answer
validated by the rules. The brain is optimized in the `dev` profile
(`[profile.dev.package."*"]`) so that the games take seconds.

## Licence

Apache-2.0. Part of the [Sashité](https://sashite.com/) project.
