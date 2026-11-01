# tape-decode development rules

## General practices (own development and deliberate divergences)

- `Result` is only for data-dependent failures (the signal, I/O). Anything
  that can only go wrong through an invalid or inconsistent `DecoderSpec` is
  an invariant: validate it once at spec construction, then
  `expect`/`assert!`/`unreachable!` with the invariant stated. No `Result`
  plumbing for invariants.
- The sample domain is f32. Use f64 only where precision demonstrably needs
  it (phase integrals over a field, least-squares normal equations, values
  above 2^24 such as sample rates), keep it in scalars or line-sized windows,
  and never materialize a field-sized f64 buffer: stream the computation.
  Construction-time filter design may be f64.
- Filters are SOS; no transfer-function (b, a) filters.
- Avoid interfield state. Decide from the current field where the
  information is there (e.g. odd/even line clustering instead of a
  flywheel). State that must persist lives in the decoder's explicit
  inter-field structs and must stay correct when multithreaded workers start
  fresh mid-tape; canonicalize at write time rather than letting output
  depend on a worker-local count.
- Hot kernels live in `optimized/`: explicit SIMD where the compiler bundles
  too narrowly, lookup tables sized to stay cache resident, bounds
  established by construction rather than per-sample checks.
- Don't carry fields, parameters or options that nothing reads; delete
  machinery the moment it becomes dead.
- CLI is conventional clap: `--kebab-case`, value enums. Profiles express
  format differences; options express user choices.
- Code reads as the plain correct behaviour: no tombstones, no history, no
  remarks about what some other implementation does. That story belongs in
  the commit body.
- One commit per logical change.

## Porting from vhs-decode

- Port the algorithmic intent, never the Python shape; where upstream is
  clumsy or handles scenarios that never happen, do it the clean way under
  the general practices above.
- Keep upstream bugs unless a fix would stay within the compare tolerance of
  upstream's own output. A deliberate deviation is documented in the commit
  body, naming the upstream commit, and nowhere in the code.
- No upstream CLI compatibility; each ported upstream option gets its clap
  shape, and options upstream has made dead are removed rather than kept.
- Name the covered upstream commits (hash and subject) in every porting
  commit, and keep the README's upstream commit link current.
