# globazog usability friction log

Captured 2026-08-16 while switching the `tpu` crate from `globset` + `walkdir`
to [`globazog`](https://crates.io/crates/globazog) 0.2 for globbing and
directory enumeration. These are the rough edges hit during the port, recorded
so the globazog API can be made easier to adopt. Each item names the concrete
pain and a suggested fix.

## 1. No full path on matches
`CqItem::Match` carries only the leaf `name`; you reconstruct full paths
yourself by maintaining a `ContainerId -> (parent, name)` map from the
`ContainerEnter` stream and walking to a `ContainerName::Root`, then resolving
the root index via `QueryHandle::roots()`. walkdir hands you `entry.path()`
directly. This was the single biggest porting cost — every call site that just
wants "give me the matching file paths" has to implement the reconstruction
bookkeeping. A convenience `Match::path()` or an opt-in "ship full path"
result-shape would remove nearly all of the adapter code.

## 2. Ring/threaded streaming model for a simple synchronous "list files"
The simplest use (collect all files matching a glob under a dir) requires
`submit()` + a `wait_pop` loop + handling the `ContainerEnter` / `ContainerEnd`
/ `Error` / `Blocked` / `Terminal` variants. globset + walkdir was a two-liner.
A blocking `collect()` / iterator adapter that yields `(PathBuf, meta)` and
folds errors into the item stream would cover the 90% case.

## 3. No standalone path matcher in the ergonomic surface
globset's `Glob::new(p).compile_matcher()` gives an `is_match(path)` you can
apply to arbitrary paths not tied to a walk (tpu uses this for `.gitignore`
filtering). globazog couples matching to the traversal engine; the standalone
matcher lives deep in `syntax::matcher::match_path` over a 32-bit codepoint IR
(`parse` -> `Pattern` -> `split_segments`), which is a lot to wire up versus
`Glob::new`. A public `Pattern::compile(text, dialect)` + `pattern.is_match(&str
/ &Path)` would help.

## 4. Descend/emit predicates can't express a negated name-in-set ergonomically
The walkdir `filter_entry(|e| !is_skipped_dir(e))` idiom (don't descend into
`.git` / `node_modules` / `target`) maps to a `descend` conjunction, but every
`Leaf::name_*` constructor (`name_exact`, `name_glob`, `name_in_set`, ...)
hardcodes `negate: false`. To get "descend only if name != X" you must build
`Leaf::Name { seg, case, negate: true }` by hand, and `seg: Segment` is only
obtainable by calling the low-level `syntax::parse::parse` and destructuring
`PatternSegment::Match`. A `Leaf::name_not_in_set(&[&str])` or a `.negated()`
combinator would make dir-pruning trivial.

## 5. `Name` -> path is lossy
Only `Name::to_string_lossy()` (U+FFFD on unmappable units) is offered; there is
no `Name -> OsString/PathBuf`. Combined with #1, reconstructing an *exact*
on-disk path from the ring is impossible in the general (non-UTF-8 /
unpaired-surrogate) case. walkdir's `entry.path()` is exact.

## 6. Two MetaMask bits (TYPE, REPARSE) needed just to classify a match
To tell a file from a dir/symlink on a match you must remember to
`.result_shape(MetaMask::TYPE | MetaMask::REPARSE)`, otherwise `meta.entry_type`
/ `meta.is_reparse` may be unset. walkdir's `entry.file_type()` is always there.
A default result-shape that includes TYPE (cheap — it comes from readdir
`d_type`) would remove a footgun.

## 7. Standalone glob matching (no walk) is low-level
For `.gitignore`-style filtering of already-known relative paths I needed
`syntax::set::PatternSet` plus manual conversion of each path into
`Vec<Vec<CodePoint>>` (u32 per char, split on separators) then `Vec<&[CodePoint]>`
to call `.matches()`. Compare globset's `Glob::new(p).compile_matcher()
.is_match(path)`. A `PatternSet::matches_path(&str)` / `matches_os(&Path)`
convenience would close the gap.

## 8. No `[...]` character classes
globazog dialects support `*` `?` `**` `{a,b}` but not POSIX bracket classes
`[abc]` / `[a-z]`, which globset supported. A few gitignore lines / user globs
that use them silently fail to compile (I skip them). Not necessarily a bug, but
a migration gotcha.

## 9. Absolute-vs-relative root ergonomics
Roots MUST be absolute (you resolve CWD yourself), and a relative pattern needs
an explicit `.root()` while an absolute pattern self-roots — two code paths for
what walkdir did with one `WalkDir::new(path)`. Reconstructing paths *relative
to the root* (to preserve the caller's original relative prefix like `./sub/f`)
also isn't offered; you rebuild it from the container chain.

## 10. MSRV jump to 1.97 (edition 2024 crate)
globazog (all published versions, including 0.1.1 and 0.2.0) requires
rustc >= 1.97. tpu-rs was pinned at MSRV 1.96 (`rust-toolchain.toml` channel
1.96.0 + workspace `rust-version = "1.96"`). Adopting globazog forced bumping
both (and the CI MSRV job) to 1.97 in lock-step. Not a library-API issue per
se, but a real adoption cost — a lower-MSRV build (e.g. keep supporting 1.96 for
a while) would widen adoption.
