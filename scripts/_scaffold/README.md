# scripts/_scaffold — one-off migration scripts, already applied

Nothing in here is a deliverable and nothing in here is run by CI. These are
the scripts used to land one specific change — the `db-get` → `db-get-raw`
rename and the §3l documentation that came with it — kept as a record of *how*
it was done, because the rename was not a pure search-and-replace and the
reasoning is worth more than the diff.

**The edits they made are in the git history, not in these files.** Running them
again is harmless (each is idempotent by construction) and pointless.

| script | what it did and why it wasn't a sed |
|---|---|
| `rename-get-raw.py` | Confined the rename to the three *byte-layer* files (`db_crash.rs`, `db_builtins.rs`, `examples/corpus/storage.ainl`) and deliberately skipped `db_refusal.rs` and `db_kv.rs`, which name `db-get` on purpose — the first because it probes the refusal matrix by symbol, the second because it is the value layer's own suite. A global rename would have broken both. |
| `tidy-get-raw.py` | Fixed the two artefacts the rename left behind: a double space where a call used to be, and **expected error strings that still named the old builtin**. The second is the one worth remembering — a mechanical rename updates call sites and leaves assertions behind, so the suite fails on the assertion rather than on the thing the assertion is about. |
| `fix-3k-docs.py` | Updated §3k's prose, scoped to the `## 3k` block so the edit could not reach §3l. |
| `tidy-3k-docs.py` | Folded in the note that explains the rename, dropped a sentence the insertion had duplicated, and fixed the heading. |
| `insert-kv-docs.py` | Inserted §3l before `## 4. Canonical examples`, with the insertion point expressed as a *rule* so a second run is a no-op. A 170-line hand-placed section is easy to insert twice. |
| `probe-aot-error-suffix.sh` | Measured the pre-existing AOT gap that made byte-for-byte refusal comparison impossible: the compiled binary's runtime errors carry no `at line L, col C (byte B)` suffix and word two shared type errors differently. That measurement is what scoped the refusal assertions in `db_kv_aot.rs` to per-backend, and it is why that file documents the divergence rather than hiding it. |

## The three lessons, restated

1. **Scope a rename by file, not by pattern.** Two files in the same tree name
   the old symbol on purpose, and only one of them is about the thing being
   renamed.
2. **A rename must sweep the assertions.** The expected *error strings* embed
   the builtin's name, so they are call sites in everything but name.
3. **Measure a backend's divergence before you promise parity.** The AOT
   error-text gap predates this work and applies to every builtin; promising
   byte-identical refusals would have been a false claim, and the fix belongs
   to a change that touches the AOT error path for all of them.

`scripts/measure-prelude.sh` and `scripts/update-builtin-counts.py` are *not*
here — they are live. The first measures the prelude's real size, the second
writes the measured numbers into the README and the site, and 4.1 shipped a
hand-computed count that was wrong, which is why they exist.
