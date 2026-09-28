# Changelog

All notable changes to this crate are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — 2026-09-28

The sparring partner: the reference search of `sashite-sanki-player` as an
engine for [SEI 1.0.0](https://sashite.dev/specs/sei/1.0.0/), for Sanki
(`sashite.sanki.kernel/1`). The protocol side — the transport, the
envelope, the validation of every request and its tests — is that of
`sanki-sei-random-engine` 0.1.0, which this crate is forked from.

### Added

- The search on a thread of its own, interrupted through the flag the brain
  polls at every node: `ping` answered at its receipt while a search runs,
  any other request ending the search first (§7), the two threads writing
  one event per lock.
- The clock, planned into one stop instant: the hard bound
  `own.deadline − overhead` less a margin (a tenth of it, 30 to 250 ms),
  this move's share of the period (`remaining / togo`, or a twenty-fifth
  of `remaining`, plus the increment, floored on a twenty-fifth of what the
  deadline counts beyond the period; three quarters of it spent),
  `movetime` as a cap; `depth` and `nodes` as hard caps, binding on their
  own without a clock; the `depth` option as the ceiling under a clock and
  of an infinite search, which runs to it and holds its `done` until the
  next request.
- The safety-net `info`: the brain's depth-1 choice, exempt from every stop,
  emitted before the search proper.
- The options `depth` (1 … 12, default 4) and `seed` (0 draws one from the
  clock; a configured seed with a cap on the depth replays the same game).
- `roots`: the replies searched one ply shallower from the other side, in
  rounds of increasing depth over every root, the best of the negations of
  the last complete round played, its mate distance counted from this
  side, the node budget shared.
- The scores: `cp` from the mover's viewpoint, `mate` in signed half-moves,
  `wdl` as a bounded, antisymmetric calibration (a logistic on `cp` with a
  scale of 400, a draw share of two fifths at equality).
- Tests that drive the binary as a host does, the random engine's kept and
  these added: the deadline less the overhead, the plan under a quota,
  `movetime`, `nodes` and `depth` caps, `ping` during a long search and the
  bounded stop of `cancel`, a seeded replay at a depth, mates in one and in
  three scored as `mate` (with and without `roots`), `roots` ranked whatever
  their order and the bound, games at depth 2 on the nine pairings; unit
  tests of the plan and of the scores.
