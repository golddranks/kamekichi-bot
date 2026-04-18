# Review Notes

META: This note is for code reviewers. The "Overall" section is for documenting how thoroughly and from what viewpoints the code is reviewed or audited. The "kept as-is" section is meant mainly for AI reviewers, to document cases that seem like issues to the reviewer, but don't warrant, in the human author's opinion, code changes, additional comments, doc changes, tests, asserts etc. When adding issues, one should explain and justify from the reviewer's perspective, what the perceived problem was, and then justify, from the author's perspective, why this isn't actually a problem. This is to push back "sloppification" effects of AI reviews – fixing non-existing problems, or flip-flopping between the "best" solution between sessions.

NOTE for reviewers: when adding items to as-is section, fill in the "problem because" part, and leave "not a problem because" for the human author to fill in.

## MODULES

### Module `src/read_buf.rs` — reviewed on 2026-04-16 at 76c4013

Comprehensive review: architecture, naming, docs, style, correctness, panics, overflows. Reviewed thoroughly both by human author and Claude Code.

#### Overall

Well-structured module. The ASCII layout diagram and module-level docs are excellent. The API surface is minimal and the separation of concerns (buffer doesn't make policy decisions about when to compact/shrink) is clean. No correctness bugs found.

#### Reviewed and kept as-is

1. `maybe_shrink_capacity` side-effect

- **Problem because:** `maybe_shrink_capacity` side-effect on initialized region — shrinking destroys the pre-initialized `end..len` region, meaning the next fill pays re-initialization cost.
- **Not a problem because:** Losing pre-initialized space is inherent to what "shrink the backing allocation" even means.

2. `read_until` with `limit = 0`

- **Problem because:** If there's no pending data, `read_until` returns `LimitReached` without ever calling the callback. Flagged as a surprising edge case.
- **Not a problem because:** "zero search budget → nothing found" is the correct semantic; besides, callers don't pass zero in practice.

3. `saturating_add` overflow masking

- **Problem because:** In `fill_from` / `ensure_initialized` — if `start + need` overflows `usize`, `saturating_add` silently produces `usize::MAX`, leading to OOM instead of a clear error. Flagged because it hides a bug at the call site.
- **Not a problem because:** The library has no way to determine what's "too much" memory; validation of wire-format lengths (e.g. u64 frame size narrowed to usize) belongs at the caller boundary, not inside the buffer.

4. `consume` panic/clamp semantics

- **Problem because:** `debug_assert` + `saturating_add` + `.min(self.end)` means in release builds an out-of-bounds `n` silently clamps rather than panicking — potentially masking caller bugs.
- **Not a problem because:** The doc intentionally says "May or may not panic" to avoid committing to either behavior — the clamping-vs-panicking distinction is an implementation detail callers must not rely on. The semantics of consume are simple: consume the still-unconsumed data. If you try to consume more than there exists, only so much can get consumed. The debug assert is to catch bugs while testing, but because the semantics are simple and the "pending" simply gets empty, it's better to accept that instead of crashing the whole program.

5. `slice()` can index into initialized-but-never-written memory

- **Problem because:** `buf.slice(end..end+10)` returns zeroes rather than panicking, so a caller could silently read unwritten data.
- **Not a problem because:** The doc says "the initialized part of the backing buffer" — accessing the initialized-but-unwritten region is documented and intended behavior, not an accident.

6. `read_until` limit counts pending bytes, not total bytes read

- **Problem because:** The limit check is `self.end - self.start >= limit`, which measures pending bytes, not total bytes read from the stream. Each `read_until` call gets a fresh budget, so successive calls can each read up to `limit` bytes. Flagged as potentially surprising if someone expects `limit` to cap cumulative I/O across calls.
- **Not a problem because:** The limit bounds a single `read_until` call, and includes existing "pending bytes". Works as intended.

7. `ensure_initialized` allocates more than the requested `target`

- **Problem because:** Name suggests "at least target" but it silently adds headroom beyond `target`.
- **Not a problem because:** The documented semantics, "initialized to at least `target`" means precisely what it says: "at least". Not "exactly".

8. Test coverage appears to lack dedicated unit tests for specific paths

- **Problem because:** No isolated unit tests for `compact` with `copy_within`, `maybe_compact` returning `false`, `consume(0)`, etc.
- **Not a problem because:** 100% line coverage already exists through the protocol-level tests in `tests.rs`. Coverage is coverage regardless of test granularity.

10. `clear` vs `compact`-when-all-consumed semantic overlap

- **Problem because:** When all data is consumed (`start == end`), both `clear()` and `compact()` produce the same state (`start = 0, end = 0`, buffer untouched). No doc distinguishes when to prefer one over the other.
- **Not a problem because:** The caller should call whichever function that has the semantic that fits their intended use. The functions have different semantics, and I don't see why an overlap in a single special case would be a problem.

11. No `#[must_use]` on `maybe_compact`

- **Problem because:** The return value gates whether `maybe_shrink_capacity` is safe to call (it requires `start == 0`). Ignoring the return value could lead to calling `maybe_shrink_capacity` without compaction having occurred, hitting the debug assert. `#[must_use]` would guard against this at compile time.
- **Not a problem because:** `#[must_use]` would imply that the caller is expected or required to call `maybe_shrink_capacity`, but that is not the case. The implication goes the other way: if the caller calls `maybe_shrink_capacity`, they should do it right after calling `maybe_compact`.

12. Error types lack `Display` / `std::error::Error` impls

- **Problem because:** `FillError` and `ReadUntilError` derive `Debug` but implement neither `Display` nor `std::error::Error`. This prevents using `?` to propagate them into `Box<dyn Error>` or similar trait-object error types.
- **Not a problem because:** Author doesn't think this is a problem without a concerete use case within the library.

13. `read_until` gives `f` one free call on pre-existing data before the limit check

- **Problem because:** If pending data already exceeds `limit` when `read_until` is entered, `f` gets called once, then the loop immediately returns `LimitReached`. This means `f` sees data beyond `limit` exactly once — an asymmetry between "data arrived via prior reads" and "data arrived within this call."
- **Not a problem because:** This is exactly how it supposed to work. The function is meant for searching for a pattern in the incoming data. If the data containing the pattern is already read in the buffer, `f` being called once is the correct behaviour.

14. `fill_from` with `need = 0` is a silent no-op

- **Problem because:** `saturating_add(0)` → `target = start`, and `end >= start` is always true, so `fill_from(reader, 0)` returns `Ok(())` without reading. A caller expecting "ensure there's _some_ data" might be surprised.
- **Not a problem because:** Those are the exact semantics of the function. If 0 additional bytes is needed, that means that the function is fine as a no-op.

15. `MIN_READ_SLICE` / `MIN_READ_HEADROOM` naming

- **Problem because:** The names describe what they ensure (minimum slice size, headroom) but not _why_. Not immediately clear from the name alone what they're preventing.
- **Not a problem because:** That's why they have comments that explain the way.

16. `maybe_compact` doc wording said "exceeds both"

- **Problem because:** The doc said "exceeds both `threshold` and half of total data read" for a `max()` condition. "Both" is technically correct (exceeding the max implies exceeding both) but could be misread as "exceeds either."
- **Not a problem because:** Like you say, "both" is technically correct, and I don't find it especially confusing. It's not the doc's fault if somebody misreads a fairly standard phrase.

17. `read_until` doc doesn't hint at how the callback manages cursor state

- **Problem because:** The doc says "the callback must manage the cursor position itself, as it can't call mutable methods on `ReadBuf`" but doesn't hint at _how_. The actual usage in `proto.rs:440–468` does it via captured variables (`scan`, `prev_nl`, `n_lines`). A parenthetical like "(e.g., via captured variables)" would close the gap for a reader who hasn't seen the call site yet.
- **Not a problem because:** Hint about incremental reads and managing your own cursors is just a sidenote, I don't want to spill lines for a full blown tutorial. The callback is `FnMut`, so that's easy enough to figure out.

18. `read_until` safety comment says `target < buf.len()` — technically correct but glosses over the chain

- **Problem because:** The comment claims `ensure_initialized` ensures `target < buf.len()`. The actual guarantee is stronger: `target + 512 <= buf.len()`, so `target < buf.len()` follows but the comment doesn't show the intermediate step. A reader verifying the safety argument has to go read `ensure_initialized` to confirm.
- **Not a problem because:** `target == buf.len()` NEVER happens. `<` is the simplest and correct in the best way, that is, technically correct. `<=` would be downright misleading.

19. `ensure_initialized` — the `max` expression is the hardest line to parse in the module

- **Problem because:** `target.saturating_add(MIN_READ_HEADROOM).max(self.end.saturating_add(MIN_READ_SLICE))` — the two arms serve different purposes (headroom past target vs. minimum readable slice past `end`) but the `max` merges them into one expression without annotation. A reader has to decompose it mentally.
- **Not a problem because:** It's one line with two clearly named constant. Should be straightforward enough.

20. `fill_from` doc says "same `need`" for retries — implies stricter contract than exists

- **Problem because:** "retrying with the same `need` resumes where the previous attempt left off" is true but suggests the caller _must_ pass the same `need`. A different `need` also works correctly since buffered bytes persist. The doc implies a requirement that doesn't exist.
- **Not a problem because:** The doc says "so retrying with the same `need` resumes where the previous attempt left off". This is an implication, an explanation of a case. Nowhere does anybody suggest that the caller _must_ pass the same `need`.

21. `maybe_compact` threshold is effectively ignored for small buffers

- **Problem because:** `self.start > threshold.max(self.end / 2)` — when `end` is small (say 100) and `threshold` is 8192, the condition requires `start > 8192`, which is impossible. So for small buffers, compaction only triggers via the `start == end` fast path. The doc doesn't call out this interaction.
- **Not a problem because:** That's exactly its purpose. The docs don't call out this interaction because it's a inane detail that the user doesn't have to care about.

22. `maybe_shrink_capacity` has no way to force a shrink

- **Problem because:** The probabilistic `one_in_eight_odds()` gate means a caller with a known-idle connection can't deterministically reclaim memory. There's no `force` parameter or separate non-probabilistic method.
- **Not a problem because:** YAGNI.

23. `read_until` safety comment — `ensure_initialized` invariant chain

- **Problem because:** The safety comment says "`ensure_initialized` ensures `target < buf.len()`" but doesn't show _why_. A reviewer might worry this breaks if `MIN_READ_HEADROOM` is zero. The actual chain: `read_once` needs `end < buf.len()`; the loop guard gives `end < target`; `ensure_initialized` gives `buf.len() >= target`. So `end < target <= buf.len()`, thus `end < buf.len()`. This holds even if both `MIN_READ_HEADROOM` and `MIN_READ_SLICE` are zero — the `>=` is sufficient, strict `<` between `target` and `buf.len()` is not required.
- **Not a problem because:** The safety comment has TWO strict unequalities
  like a < b < c. Only one is required to be strict to prove a < c. So if the
  reviewer worries that b < c might not hold in some hypothetical future where
  for some unfathomable reason a maintainer decides to set a const to zero, there is still the other, a < b inequality. Not worth worrying about.

25. `len` label in the ASCII layout diagram

- **Problem because:** The diagram uses `len` for the initialized-region boundary (`buf.len()`), but the module has no `len` field — and `len` conventionally suggests "amount of data." A reader's first pass may confuse it with pending data length. A label like `init` would map more clearly to the concept.
- **Not a problem because:** `len` is a field on the underlying `Vec`, plus a
  method on it. I can't see any other, more fitting name, especially as we can't
  rename `Vec`'s internals.

26. `WouldBlock` variant name covers both `WouldBlock` and `TimedOut`

- **Problem because:** `FillError::WouldBlock` and `ReadUntilError::WouldBlock` are returned for both `io::ErrorKind::WouldBlock` and `io::ErrorKind::TimedOut`. The name only reflects one of the two cases. A caller pattern-matching on the variant might not realize timeouts are folded in.
- **Not a problem because:** These two error cases are conflated on different
  operating systems, so merging them into one is the portable solution. Both signal the same thing: no serious error has happened, so retry later, and it might succeed.

27. `ReadUntilError::CallbackError` variant naming asymmetry

- **Problem because:** Other variants of `ReadUntilError` are `Eof`, `Io`, `WouldBlock`, `LimitReached` — none carry an `Error` suffix. `CallbackError(E)` is the odd one out. `Callback(E)` would match the naming pattern of the sibling variants.
- **Not a problem because:** `CallbackError` differs from the others in the sense that it's not a specific error, but "some" error decided by the caller.
  Naming the error just as "Callback" wouldn't convey that it is, indeed,
  an error case, unlike the other variants. The Io variant is arguably similar,
  but common enough, plus unlike "callback", names a _domain_, that I'm happy
  with it.

28. `maybe_shrink_capacity` debug/release divergence

- **Problem because:** `maybe_shrink_capacity` has `debug_assert!(self.start == 0, "call compact() before shrinking")` paired with a runtime `if self.start == 0 && ...` gate. In debug builds, calling without prior compaction panics; in release builds, the call silently no-ops. Same hybrid pattern as `consume` (#4): an invariant-violating caller gets different behavior depending on build mode, which can mask bugs that only surface in release.
- **Not a problem because:** I call these "soft invariants", and they are commonly used both in Rust (integer over/underflows, panicing on debug, wrapping in release) and in Linux kernel "don't kernel panic if you can go on, even if in a slightly degraded state". For some reason, the AI reviewer seems to have an allergy against them. The point is to crash early, IN TESTING, where crashes can be tolerated easily. If there is a "natural" no-op state or default action that could be taken instead, opt to do that in release mode. If not, _then_ consider crashing. The review messages sometimes say that this pattern "shows up green in CI, and silently corrupts in production". No. It loudly crashes in CI, and does no harm in production. That's the point.
