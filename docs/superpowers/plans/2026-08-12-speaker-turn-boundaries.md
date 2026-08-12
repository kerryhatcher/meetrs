# Speaker-Turn Boundaries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Label each transcript segment with a chunk-local speaker-turn number derived from whisper.cpp's tinydiarize, on both audio legs.

**Architecture:** `transcribe_leg` splits into a thin whisper-FFI collection loop producing `RawSegment`, plus a pure `number_turns` that filters non-speech and assigns monotonic turn numbers. A single `speaker_label` helper renders `leg` + turn for every output sink so they cannot drift. The default model changes to `ggml-small.en-tdrz.bin`, and `tdrz_enable` is switched on unconditionally.

**Tech Stack:** Rust 2024, whisper-rs 0.16 (vendored whisper.cpp, Metal), rusqlite 0.40 (bundled SQLite + FTS5), serde_json, anyhow.

## Global Constraints

- Platform: macOS only. macOS 14.4+ floor (Core Audio process-tap requirement).
- Turn numbers are **chunk-local and per-leg**. Counters reset on every `transcribe_leg` call. Never imply cross-chunk or cross-leg identity.
- Numbering is **monotonic** (`T1, T2, T3…`), never alternating. Adjacent turns differ; nothing else is claimed.
- `turn` is `Option<u32>` end to end. `None` means "unknown" — either a pre-existing row or a non-tdrz model.
- Label format: `"system/T3"` when known, bare `"system"` when not.
- `just lint` runs `cargo clippy --all-targets -- -D warnings`. Zero warnings.
- `just test` runs `cargo test`. All tests pass at the end of every task.
- Conventional Commits, imperative mood, lowercase, no trailing period. Commits are GPG-signed automatically.
- Do not add dependencies. Everything needed is already in `Cargo.toml`.
- Do not touch `src/capture.rs`, `src/chunk.rs`, `src/vad.rs`, `src/ui.rs`, or `src/types.rs`.

### Verified facts (do not re-derive)

- `WhisperSegment::next_segment_speaker_turn() -> bool` exists at `whisper-rs-0.16.0/src/whisper_state/segment.rs:83`.
- `FullParams::set_tdrz_enable(bool)` exists at `whisper-rs-0.16.0/src/whisper_params.rs:262`.
- Model: `https://huggingface.co/akashmjn/tinydiarize-whisper.cpp/resolve/main/ggml-small.en-tdrz.bin`, 487,614,184 bytes, sha256 `ceac3ec06d1d98ef71aec665283564631055fd6129b79d8e1be4f9cc33cc54b4`. It is **not** in `ggerganov/whisper.cpp` (404), and no quantized variant exists.
- The `[_SOLM_]` turn token is id 50359, above `token_eot`, so with the existing `set_print_special(false)` it never leaks into segment text (`whisper.cpp:7615`). No text filtering is needed for it.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/transcribe.rs` | Whisper FFI, turn numbering, label rendering, all four output sinks | Modify — the bulk of the work |
| `src/db.rs` | Schema, migration, segment persistence, FTS search | Modify — schema v2 + `turn` plumbing |
| `src/main.rs` | CLI, `--search` output rendering | Modify — one render site |
| `src/model.rs` | Model download + checksum cache | Modify — three constants |
| `README.md` | User-facing docs | Modify — model size note |

No new files. `speaker_label` and `number_turns` live in `src/transcribe.rs` beside the existing pure helpers (`downmix_leg`, `is_non_speech_marker`, `session_relative_secs`), matching how that file is already factored.

---

## Task 1: The `speaker_label` helper

**Files:**
- Modify: `src/transcribe.rs` (add helper next to `session_relative_secs` at line 106; tests in the existing `mod tests` at the end of the file)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub(crate) fn speaker_label(leg: &str, turn: Option<u32>) -> String`. Task 2 uses it for `transcript.md` and TUI lines; Task 3's consumer in `main.rs` uses it for `--search` output.

`pub(crate)` because `src/main.rs` calls it across the module boundary. All modules in this crate are private (`mod transcribe;` in `main.rs:15`), so `pub(crate)` is the correct visibility — plain `pub` would trip clippy's unreachable-pub lints in some configurations.

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` block at the bottom of `src/transcribe.rs`:

```rust
    #[test]
    fn speaker_label_appends_the_turn_when_known() {
        assert_eq!(speaker_label("system", Some(3)), "system/T3");
        assert_eq!(speaker_label("mic", Some(1)), "mic/T1");
    }

    #[test]
    fn speaker_label_is_bare_when_the_turn_is_unknown() {
        // None means "this model cannot diarize" or "recorded before turns
        // existed". Rendering T1 there would assert something we do not know.
        assert_eq!(speaker_label("system", None), "system");
        assert_eq!(speaker_label("mic", None), "mic");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test speaker_label`
Expected: FAIL — `cannot find function 'speaker_label' in this scope`

- [ ] **Step 3: Write the implementation**

Insert into `src/transcribe.rs` immediately after `session_relative_secs` (after line 108):

```rust
/// How a segment's speaker is named in every output sink. `None` renders bare:
/// turn numbers are a claim, and we only make it when tinydiarize supplied one.
///
/// Turn numbers are chunk-local and per-leg — `system/T1` in chunk 3 is not the
/// same person as `system/T1` in chunk 4. Collapsing turns into stable speakers
/// needs embeddings and is deliberately out of scope here.
pub(crate) fn speaker_label(leg: &str, turn: Option<u32>) -> String {
    match turn {
        Some(n) => format!("{leg}/T{n}"),
        None => leg.to_string(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test speaker_label`
Expected: PASS, 2 tests

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt
just lint
git add src/transcribe.rs
git commit -m "feat(transcribe): add speaker_label for leg + turn rendering"
```

---

## Task 2: `RawSegment` and `number_turns`

**Files:**
- Modify: `src/transcribe.rs` — `SegmentOut` (line 79-86), `transcribe_leg` (line 131-167), `process_job` (line 195-227), the render loop in `run` (line 336-347); tests in `mod tests`

**Interfaces:**
- Consumes: `speaker_label(&str, Option<u32>) -> String` from Task 1; the existing `is_non_speech_marker(&str) -> bool` and `session_relative_secs(Duration, f64) -> f64`.
- Produces:
  - `struct RawSegment { text: String, start_cs: i64, end_cs: i64, no_speech_prob: f32, turn_flag: bool }`
  - `fn number_turns(raws: Vec<RawSegment>, leg: &'static str, offset: Duration, tdrz_available: bool) -> Vec<SegmentOut>`
  - `SegmentOut` gains `turn: Option<u32>`
  - `transcribe_leg` gains a trailing `tdrz_available: bool` parameter
  - `process_job` gains a trailing `tdrz_available: bool` parameter

At the end of this task `tdrz_available` is hard-wired to `false` at the single call site in `run`, so behaviour is unchanged apart from `"turn": null` appearing in `chunk-NNN.json`. Task 4 flips it on. This keeps the task independently verifiable without a 465 MiB download.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/transcribe.rs`:

```rust
    fn raw(text: &str, start_cs: i64, turn_flag: bool) -> RawSegment {
        RawSegment {
            text: text.into(),
            start_cs,
            end_cs: start_cs + 100,
            no_speech_prob: 0.01,
            turn_flag,
        }
    }

    fn turns(out: &[SegmentOut]) -> Vec<Option<u32>> {
        out.iter().map(|s| s.turn).collect()
    }

    #[test]
    fn number_turns_with_no_flags_puts_everything_in_turn_one() {
        let raws = vec![
            raw("one", 0, false),
            raw("two", 100, false),
            raw("three", 200, false),
        ];
        let out = number_turns(raws, "system", Duration::ZERO, true);
        assert_eq!(turns(&out), vec![Some(1), Some(1), Some(1)]);
    }

    #[test]
    fn number_turns_increments_after_the_flagged_segment_not_before() {
        // tdrz flags "a change happens AFTER this segment", so the flagged
        // segment itself still belongs to the outgoing turn.
        let raws = vec![raw("a", 0, true), raw("b", 100, false), raw("c", 200, false)];
        let out = number_turns(raws, "system", Duration::ZERO, true);
        assert_eq!(turns(&out), vec![Some(1), Some(2), Some(2)]);
    }

    #[test]
    fn a_turn_flagged_on_a_filtered_segment_still_advances_the_counter() {
        // Regression guard. A pause between two speakers is exactly where a
        // non-speech marker and a turn change coincide. Incrementing inside the
        // filter would swallow the boundary and merge two speakers into T1.
        let raws = vec![
            raw("before", 0, false),
            raw("[BLANK_AUDIO]", 100, true),
            raw("after", 200, false),
        ];
        let out = number_turns(raws, "system", Duration::ZERO, true);
        assert_eq!(out.len(), 2, "the marker segment must not be emitted");
        assert_eq!(turns(&out), vec![Some(1), Some(2)]);
    }

    #[test]
    fn a_flag_on_the_final_segment_creates_no_phantom_turn() {
        let raws = vec![raw("a", 0, false), raw("b", 100, true)];
        let out = number_turns(raws, "system", Duration::ZERO, true);
        assert_eq!(out.len(), 2);
        assert_eq!(turns(&out), vec![Some(1), Some(1)]);
    }

    #[test]
    fn turns_are_none_when_the_model_cannot_diarize() {
        let raws = vec![raw("a", 0, true), raw("b", 100, false)];
        let out = number_turns(raws, "system", Duration::ZERO, false);
        assert_eq!(turns(&out), vec![None, None]);
    }

    #[test]
    fn number_turns_on_empty_input_is_empty() {
        assert!(number_turns(Vec::new(), "mic", Duration::ZERO, true).is_empty());
    }

    #[test]
    fn number_turns_makes_timestamps_session_relative() {
        // 250 centiseconds = 2.5s into a chunk that starts 60s into the session.
        let out = number_turns(vec![raw("a", 250, false)], "mic", Duration::from_secs(60), true);
        assert_eq!(out[0].start_secs, 62.5);
        assert_eq!(out[0].end_secs, 63.5);
        assert_eq!(out[0].leg, "mic");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test number_turns`
Expected: FAIL — `cannot find struct 'RawSegment'` / `cannot find function 'number_turns'`

- [ ] **Step 3: Add the `turn` field to `SegmentOut`**

Replace `src/transcribe.rs` lines 78-86 with:

```rust
/// One transcribed segment, session-relative in time.
#[derive(Serialize, Clone)]
struct SegmentOut {
    leg: &'static str,
    start_secs: f64,
    end_secs: f64,
    text: String,
    no_speech_prob: f32,
    /// Chunk-local, per-leg speaker turn, 1-based. `None` when the model cannot
    /// diarize. Never compare across chunks or legs.
    turn: Option<u32>,
}
```

- [ ] **Step 4: Add `RawSegment` and `number_turns`**

Insert into `src/transcribe.rs` immediately after `speaker_label` (added in Task 1):

```rust
/// One segment exactly as whisper reported it, before filtering or numbering.
/// Exists so the numbering logic is a pure function over plain data —
/// `WhisperSegment` cannot be constructed outside whisper-rs, so testing
/// against it directly is impossible.
struct RawSegment {
    text: String,
    /// Whisper's chunk-relative timestamps, in centiseconds.
    start_cs: i64,
    end_cs: i64,
    no_speech_prob: f32,
    /// tinydiarize predicted a speaker change *after* this segment.
    turn_flag: bool,
}

/// Drops non-speech segments and assigns monotonic, chunk-local turn numbers.
///
/// Monotonic rather than alternating on purpose: tdrz only establishes that turn
/// N differs from turn N+1. Whether turn 3 returns to turn 1's speaker is
/// unknowable here, and `S1/S2` alternation would assert a two-speaker
/// conversation that is false in any larger meeting.
fn number_turns(
    raws: Vec<RawSegment>,
    leg: &'static str,
    offset: Duration,
    tdrz_available: bool,
) -> Vec<SegmentOut> {
    let mut out = Vec::with_capacity(raws.len());
    let mut turn = 1u32;
    for raw in raws {
        // Whisper emits bracketed non-speech markers for silence. One leg is
        // quiet in most chunks (nobody talks over the whole meeting), so without
        // this the transcript is mostly "[BLANK_AUDIO]".
        if !is_non_speech_marker(&raw.text) {
            out.push(SegmentOut {
                leg,
                start_secs: session_relative_secs(offset, raw.start_cs as f64 * 0.01),
                end_secs: session_relative_secs(offset, raw.end_cs as f64 * 0.01),
                text: raw.text,
                no_speech_prob: raw.no_speech_prob,
                turn: tdrz_available.then_some(turn),
            });
        }
        // Outside the filter: a boundary flagged on a dropped marker is still a
        // real boundary, and incrementing inside would silently lose it.
        if raw.turn_flag {
            turn += 1;
        }
    }
    out
}
```

- [ ] **Step 5: Run the new tests to verify they pass**

Run: `cargo test number_turns`
Expected: PASS, 6 tests (`number_turns_*`, `a_turn_flagged_*`, `a_flag_on_the_final_*`, `turns_are_none_*`)

- [ ] **Step 6: Rewrite `transcribe_leg` to use them**

Replace `src/transcribe.rs` lines 130-167 (the whole `transcribe_leg` function, from its doc comment through its closing brace) with:

```rust
/// Transcribe one mono 16kHz leg, returning session-relative segments.
fn transcribe_leg(
    ctx: &WhisperContext,
    mono_16k: &[f32],
    leg: &'static str,
    offset: Duration,
    tdrz_available: bool,
) -> Result<Vec<SegmentOut>> {
    if mono_16k.is_empty() {
        return Ok(Vec::new());
    }
    let mut state = ctx.create_state().context("creating whisper state")?;
    state
        .full(full_params(), mono_16k)
        .context("running whisper full()")?;

    let n = state.full_n_segments();
    let mut raws = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Some(seg) = state.get_segment(i) else {
            continue;
        };
        raws.push(RawSegment {
            text: seg.to_str_lossy().unwrap_or_default().trim().to_string(),
            start_cs: seg.start_timestamp(),
            end_cs: seg.end_timestamp(),
            no_speech_prob: seg.no_speech_probability(),
            turn_flag: seg.next_segment_speaker_turn(),
        });
    }
    Ok(number_turns(raws, leg, offset, tdrz_available))
}
```

- [ ] **Step 7: Thread `tdrz_available` through `process_job`**

In `src/transcribe.rs`, change the `process_job` signature (line 195) to:

```rust
fn process_job(
    ctx: &WhisperContext,
    job: &Job,
    meta: &SessionMeta,
    tdrz_available: bool,
) -> Result<Processed> {
```

and its two `transcribe_leg` calls (lines 214-215) to:

```rust
    let mut segments = transcribe_leg(ctx, &mic_16k, "mic", job.offset, tdrz_available)?;
    segments.extend(transcribe_leg(
        ctx,
        &sys_16k,
        "system",
        job.offset,
        tdrz_available,
    )?);
```

- [ ] **Step 8: Update the call site in `run` and render via `speaker_label`**

In `src/transcribe.rs`, change the `process_job` call (line 315) to pass `false` for now — Task 4 replaces this literal:

```rust
        // Task 4 replaces this with real tdrz capability detection.
        match process_job(&ctx, &job, session_meta, false) {
```

Then in the render loop (lines 337-347), replace the two `seg.leg` uses with the shared label:

```rust
                let mut lines = Vec::with_capacity(processed.segments.len());
                for seg in &processed.segments {
                    let ts = fmt_timestamp(seg.start_secs);
                    let who = speaker_label(seg.leg, seg.turn);
                    let text = polish(&spell, &sentences, &seg.text);
                    transcript_md.push_str(&format!("**[{ts}] {who}:** {text}\n\n"));
                    lines.push(format!("[{ts}] {who}: {text}"));
                }
```

- [ ] **Step 9: Run the whole suite and lint**

Run: `just test && just lint`
Expected: all tests PASS, clippy clean. The pre-existing `downmix_leg_*` tests must still pass untouched.

- [ ] **Step 10: Commit**

```bash
git add src/transcribe.rs
git commit -m "feat(transcribe): number speaker turns from tinydiarize flags

Splits transcribe_leg into a whisper-FFI collection loop and a pure
number_turns, so the numbering rules are testable without constructing a
WhisperSegment. Turn numbers stay None until the model swap lands."
```

---

## Task 3: Persist `turn` in SQLite

**Files:**
- Modify: `src/db.rs` — `migrate` (line 60-138), `SegmentIn` (line 141-147), `Hit` (line 149-157), `record_segments` (line 266-299), `search` (line 314-350), `RebuildSegment` (line 458-465) and the mapping in `rebuild` (line 407-416); tests in `mod tests`
- Modify: `src/transcribe.rs` — the `SegmentIn` construction in `run` (line 359-366)
- Modify: `src/main.rs` — `search` output (line 292-302)

**Interfaces:**
- Consumes: `SegmentOut.turn: Option<u32>` from Task 2; `speaker_label` from Task 1.
- Produces: `SegmentIn.turn: Option<u32>`, `Hit.turn: Option<u32>`, schema `user_version = 2`.

**Critical:** the existing test `migration_runs_once_and_is_idempotent` (`db.rs:536`) asserts `user_version == 1` twice. It must be updated to `2` in this task or the suite breaks.

**Also critical:** do **not** add `turn` to the `version < 1` `CREATE TABLE segments`. `migrate` reads `version` once and runs both blocks in sequence inside one transaction, so a fresh database goes `CREATE` (no turn) → `ALTER` (adds turn). Adding it in both places makes the `ALTER` fail with "duplicate column name".

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/db.rs`:

```rust
    #[test]
    fn migration_adds_the_turn_column() {
        let path = tmp_db_path("turn-column");
        let _ = std::fs::remove_file(&path);
        let db = open_at(&path).unwrap();
        let cols: Vec<String> = db
            .conn
            .prepare("SELECT name FROM pragma_table_info('segments')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(cols.contains(&"turn".to_string()), "columns were {cols:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn turn_round_trips_and_none_becomes_null() {
        let path = tmp_db_path("turn-roundtrip");
        let _ = std::fs::remove_file(&path);
        let mut db = open_at(&path).unwrap();
        let dir = PathBuf::from("/tmp/session-turn");
        db.start_session(&dir, &info(), "earshot", "2026-01-01T00:00:00Z")
            .unwrap();
        db.record_chunk(&dir, 0, "chunk-000.wav", 5.0, 0.0, 1234)
            .unwrap();

        let mut known = seg("first speaker", 0.0);
        known.turn = Some(2);
        let mut unknown = seg("unknown speaker", 1.0);
        unknown.turn = None;
        db.record_segments(&dir, 0, &[known, unknown]).unwrap();

        let stored: Vec<Option<i64>> = db
            .conn
            .prepare("SELECT turn FROM segments ORDER BY start_secs")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(stored, vec![Some(2), None]);

        let hits = db.search("speaker", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.turn == Some(2)));
        assert!(hits.iter().any(|h| h.turn.is_none()));
        let _ = std::fs::remove_file(&path);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test turn_round_trips migration_adds_the_turn_column`
Expected: FAIL — no field `turn` on `SegmentIn`, no field `turn` on `Hit`

- [ ] **Step 3: Add the migration**

In `src/db.rs`, after the `if version < 1 { ... }` block closes and **before** `tx.commit()?` (line 136), insert:

```rust
    if version < 2 {
        // Nullable on purpose: rows written before turn detection existed read
        // NULL, which is distinguishable from a real turn number. No backfill —
        // those sessions were transcribed by a model that cannot diarize.
        //
        // segments_fts is external-content over `text` alone and the triggers
        // only reference rowid/text, so none of them are affected.
        tx.execute_batch(
            r#"
            ALTER TABLE segments ADD COLUMN turn INTEGER;
            PRAGMA user_version = 2;
            "#,
        )?;
    }
```

- [ ] **Step 4: Add the fields and update the SQL**

In `src/db.rs`, add to `SegmentIn` (after `no_speech_prob`, line 146):

```rust
    /// Chunk-local, per-leg speaker turn. `None` stores NULL.
    pub turn: Option<u32>,
```

Add to `Hit` (after `leg`, line 153):

```rust
    pub turn: Option<u32>,
```

In `record_segments`, change the prepared statement and its params (lines 277-291):

```rust
            let mut stmt = tx.prepare(
                "INSERT INTO segments (chunk_id, leg, start_secs, end_secs, text, no_speech_prob, turn)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for s in segs {
                stmt.execute(params![
                    chunk_id,
                    s.leg,
                    s.start_secs,
                    s.end_secs,
                    s.text,
                    s.no_speech_prob,
                    s.turn,
                ])?;
            }
```

In `search`, add `seg.turn` to the SELECT list and read it back (lines 315-338):

```rust
        let mut stmt = self.conn.prepare(
            "SELECT s.dir, s.started_at, c.idx, seg.leg, seg.start_secs,
                    snippet(segments_fts, 0, '[', ']', '...', 10), seg.turn
             FROM segments_fts
             JOIN segments seg ON seg.id = segments_fts.rowid
             JOIN chunks c ON c.id = seg.chunk_id
             JOIN sessions s ON s.id = c.session_id
             WHERE segments_fts MATCH ?1
             ORDER BY s.started_at DESC, c.idx ASC, seg.start_secs ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], |r| {
            Ok(Hit {
                session_dir: r.get(0)?,
                started_at: r.get(1)?,
                chunk_index: r.get(2)?,
                leg: r.get(3)?,
                start_secs: r.get(4)?,
                snippet: r.get(5)?,
                turn: r.get(6)?,
            })
        });
```

- [ ] **Step 5: Update the reindex path**

In `src/db.rs`, add to `RebuildSegment` (after `no_speech_prob`, line 464):

```rust
    /// Chunk JSONs written before turn detection lack this field entirely.
    #[serde(default)]
    turn: Option<u32>,
```

and in `rebuild`'s mapping (line 407-416) add the field:

```rust
                        .map(|s| SegmentIn {
                            leg: s.leg,
                            start_secs: s.start_secs,
                            end_secs: s.end_secs,
                            text: s.text,
                            no_speech_prob: s.no_speech_prob,
                            turn: s.turn,
                        })
```

- [ ] **Step 6: Update the existing tests that now know about `turn`**

In `src/db.rs`'s `mod tests`, add the field to the `seg` helper:

```rust
    fn seg(text: &str, start: f64) -> SegmentIn {
        SegmentIn {
            leg: "mic".into(),
            start_secs: start,
            end_secs: start + 1.0,
            text: text.into(),
            no_speech_prob: 0.01,
            turn: None,
        }
    }
```

And in `migration_runs_once_and_is_idempotent` (line 536), change **both** `assert_eq!(v, 1)` and `assert_eq!(v2, 1)` to expect `2`:

```rust
        assert_eq!(v, 2);
```
```rust
        assert_eq!(v2, 2);
```

- [ ] **Step 7: Update the producer in `transcribe.rs`**

In `src/transcribe.rs`'s `run`, add the field to the `SegmentIn` construction (line 359-366):

```rust
                        .map(|s| crate::db::SegmentIn {
                            leg: s.leg.to_string(),
                            start_secs: s.start_secs,
                            end_secs: s.end_secs,
                            text: s.text.clone(),
                            no_speech_prob: s.no_speech_prob,
                            turn: s.turn,
                        })
```

- [ ] **Step 8: Render the turn in `--search` output**

In `src/main.rs`, replace the loop body at lines 292-302:

```rust
    for h in &hits {
        println!(
            "{}  {}  chunk-{:03} [{}]\n  {}\n  {}\n",
            h.started_at,
            fmt_hms(h.start_secs),
            h.chunk_index,
            crate::transcribe::speaker_label(&h.leg, h.turn),
            h.snippet.replace('\n', " "),
            h.session_dir
        );
    }
```

- [ ] **Step 9: Run the whole suite and lint**

Run: `just test && just lint`
Expected: all tests PASS including the two new ones and the updated `migration_runs_once_and_is_idempotent`; clippy clean.

- [ ] **Step 10: Verify the migration works on a real pre-existing database**

Run:
```bash
cp ~/.meetrs/meetrs.db /tmp/meetrs-pre-v2.db
sqlite3 /tmp/meetrs-pre-v2.db "PRAGMA user_version; SELECT COUNT(*) FROM segments;"
cargo run --quiet -- --search "the" | head -5
sqlite3 ~/.meetrs/meetrs.db "PRAGMA user_version; SELECT COUNT(*) FROM segments WHERE turn IS NULL;"
```
Expected: the backup reports `user_version` 1; after running, the live DB reports 2, every pre-existing row has `turn IS NULL`, and `--search` prints bare legs (no `/T`) for those rows. If anything goes wrong, `cp /tmp/meetrs-pre-v2.db ~/.meetrs/meetrs.db` restores it.

- [ ] **Step 11: Commit**

```bash
git add src/db.rs src/transcribe.rs src/main.rs
git commit -m "feat(db): store speaker turn per segment at schema v2

Nullable column so rows written before turn detection stay honestly
unknown rather than claiming turn 1. FTS is external-content over text
alone, so the virtual table and triggers are untouched."
```

---

## Task 4: Swap to the tinydiarize model and switch turns on

**Files:**
- Modify: `src/model.rs` — constants (lines 22-27)
- Modify: `src/transcribe.rs` — `full_params` (line 119-137), new `model_supports_tdrz`, `run` (line 288-315)
- Modify: `README.md`

**Interfaces:**
- Consumes: `process_job(.., tdrz_available: bool)` from Task 2.
- Produces: `fn model_supports_tdrz(model: &Path) -> bool`. Nothing later depends on it.

This is the task where turn labels start appearing, and where the 465 MiB download happens.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/transcribe.rs`:

```rust
    #[test]
    fn tdrz_support_is_detected_from_the_model_filename() {
        // There is no API to ask a GGML model whether it was trained to emit the
        // turn token, so the filename is the only signal — which is exactly what
        // whisper.cpp's own download tooling keys on.
        assert!(model_supports_tdrz(Path::new(
            "/Users/x/.meetrs/models/ggml-small.en-tdrz.bin"
        )));
        assert!(!model_supports_tdrz(Path::new(
            "/Users/x/.meetrs/models/ggml-base.en-q5_1.bin"
        )));
    }

    #[test]
    fn tdrz_detection_ignores_directories_above_the_file() {
        // A user whose model cache happens to sit under a "tdrz" directory must
        // not get turn labels from a model that cannot produce them.
        assert!(!model_supports_tdrz(Path::new("/tdrz/ggml-base.en.bin")));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test tdrz`
Expected: FAIL — `cannot find function 'model_supports_tdrz' in this scope`

- [ ] **Step 3: Implement the detection**

Insert into `src/transcribe.rs` after `number_turns`:

```rust
/// Whether this model can emit tinydiarize speaker-turn tokens.
///
/// GGML files carry no capability metadata, and `tdrz_enable` on a model that
/// was never trained to emit token 50359 simply un-suppresses a token that then
/// never fires — every segment would land in T1, which is worse than admitting
/// we do not know. Keyed on the filename, same as whisper.cpp's own tooling.
fn model_supports_tdrz(model: &Path) -> bool {
    model
        .file_name()
        .is_some_and(|n| n.to_string_lossy().contains("tdrz"))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test tdrz`
Expected: PASS, 2 tests

- [ ] **Step 5: Enable tdrz in `full_params`**

In `src/transcribe.rs`'s `full_params`, add after `params.set_translate(false);` (line 123):

```rust
    // Stops whisper.cpp suppressing token 50359 ([_SOLM_]) at -INFINITY. Safe to
    // set unconditionally: on a non-tdrz model the token is untrained and never
    // fires, and `model_supports_tdrz` is what decides whether we believe it.
    // The token sits above token_eot, so with set_print_special(false) below it
    // never leaks into segment text.
    params.set_tdrz_enable(true);
```

- [ ] **Step 6: Detect capability once per session and warn**

In `src/transcribe.rs`'s `run`, after the `WhisperContext` is constructed (line 286) and before `let mut meta`, insert:

```rust
    // Once per session, not per chunk: MEETRS_MODEL is a real escape hatch and
    // pointing it at a non-tdrz model must degrade to unlabelled turns rather
    // than to a wrong T1 on every segment.
    let tdrz_available = model_supports_tdrz(&model);
    if !tdrz_available {
        let _ = tx.send(Status::Warning(format!(
            "{} cannot detect speaker turns; transcripts will have no T-numbers. \
             Unset {} to use the default tinydiarize model.",
            model.display(),
            crate::model::MODEL_ENV
        )));
    }
```

Then replace the placeholder from Task 2 (line 315):

```rust
        match process_job(&ctx, &job, session_meta, tdrz_available) {
```

Add `use std::path::Path;` to the imports if `Path` is not already in scope — line 17 currently reads `use std::path::{Path, PathBuf};`, so it already is.

- [ ] **Step 7: Swap the model constants**

Replace `src/model.rs` lines 22-27 with:

```rust
const FILENAME: &str = "ggml-small.en-tdrz.bin";
const URL: &str =
    "https://huggingface.co/akashmjn/tinydiarize-whisper.cpp/resolve/main/ggml-small.en-tdrz.bin";
/// Verified 2026-08-12: matches the git-lfs `oid` HuggingFace's API reports for
/// this file (487,614,184 bytes).
///
/// This is the tinydiarize fine-tune of small.en, and it is the only published
/// model that emits speaker-turn tokens. It lives in akashmjn's repo rather than
/// ggerganov/whisper.cpp, which does not host it, and no quantized variant
/// exists — hence 465 MiB rather than the 57 MiB of the base.en-q5_1 it replaces.
const SHA256: &str = "ceac3ec06d1d98ef71aec665283564631055fd6129b79d8e1be4f9cc33cc54b4";
```

- [ ] **Step 8: Note the download size and the orphaned model in the README**

In `README.md`, after the `## Install` code block, add:

```markdown
First run downloads the transcription model (`ggml-small.en-tdrz.bin`, 465 MiB)
to `~/.meetrs/models/`. It is the tinydiarize fine-tune of `small.en` — the only
published Whisper model that marks speaker turns, which is why it is unquantized
and eight times the size of a plain `base.en`. Set `MEETRS_MODEL` to use a model
you already have; transcripts then simply carry no `T` numbers.

Upgrading from an earlier version leaves the superseded `ggml-base.en-q5_1.bin`
in that directory. meetrs grooms nothing, so delete it yourself if you want the
57 MiB back.
```

- [ ] **Step 9: Run the suite, lint, and trigger the download**

Run: `just test && just lint`
Expected: all tests PASS, clippy clean.

Then fetch the model and confirm the checksum path works end to end:
```bash
cargo run --quiet -- --check
ls -l ~/.meetrs/models/
```
Expected: a download progress line, then `ggml-small.en-tdrz.bin` present at exactly 487614184 bytes. A checksum failure here means the constant is wrong — do not proceed.

- [ ] **Step 10: Commit**

```bash
git add src/model.rs src/transcribe.rs README.md
git commit -m "feat(transcribe): enable tinydiarize speaker-turn detection

Swaps the default model to the tinydiarize fine-tune of small.en, the only
published model that emits turn tokens. Costs 465 MiB against base.en-q5_1's
57 MiB and buys lower WER alongside the turn markers. A non-tdrz model via
MEETRS_MODEL degrades to unlabelled turns with one warning."
```

---

## Task 5: Verify against real captured audio

**Files:**
- Modify: `src/transcribe.rs` — one `#[ignore]`d test in `mod tests`

**Interfaces:**
- Consumes: everything from Tasks 1-4.
- Produces: nothing consumed by later work.

Unit tests prove the numbering *rules*. They cannot prove tinydiarize's turn detection is any *good* — that needs real multi-speaker audio. The fixtures in `fixtures/2026-08-12T15-53-14/` (gitignored) exist for this. An `#[ignore]`d test is the harness because it reuses `process_job` directly; there is no CLI path that transcribes an arbitrary directory, and adding one is out of scope.

- [ ] **Step 1: Write the harness test**

Add to `mod tests` in `src/transcribe.rs`:

```rust
    /// Not run by `cargo test`: needs the 465 MiB model and gitignored fixture
    /// audio. Run it deliberately:
    ///   cargo test --release fixture_chunk -- --ignored --nocapture
    /// The assertions catch wiring bugs; the printed transcript is for a human
    /// to judge whether the turn boundaries land in sensible places.
    #[test]
    #[ignore = "needs the tdrz model and gitignored fixture audio"]
    fn fixture_chunk_gets_monotonic_turn_labels() {
        let dir = PathBuf::from("fixtures/2026-08-12T15-53-14");
        assert!(
            dir.join("meta.json").is_file(),
            "fixtures missing; see the spec for which chunks to copy"
        );
        let meta = load_session_meta(&dir).unwrap();
        let model = crate::model::ensure(|_, _| {}).unwrap();
        let tdrz = model_supports_tdrz(&model);
        assert!(tdrz, "expected a tdrz model, got {}", model.display());

        let ctx =
            WhisperContext::new_with_params(&model, WhisperContextParameters::default()).unwrap();

        // chunk-139 is system-leg only and densely multi-speaker: the case tdrz
        // is actually for. chunk-209 is mic-only, standing in for in-person.
        for index in [139u32, 209] {
            let job = Job {
                path: dir.join(format!("chunk-{index:03}.wav")),
                index,
                offset: Duration::ZERO,
            };
            let processed = process_job(&ctx, &job, &meta, tdrz).unwrap();
            println!("\n=== chunk-{index:03} ===");
            for seg in &processed.segments {
                println!(
                    "[{}] {}: {}",
                    fmt_timestamp(seg.start_secs),
                    speaker_label(seg.leg, seg.turn),
                    seg.text
                );
            }
            assert!(
                !processed.segments.is_empty(),
                "chunk-{index:03} produced no segments"
            );

            // Per leg, not across the whole chunk: process_job sorts segments by
            // start_secs, and the two legs number independently, so an
            // interleaved sequence legitimately reads mic/T4 then system/T1.
            for leg in ["mic", "system"] {
                let mut last: Option<u32> = None;
                for seg in processed.segments.iter().filter(|s| s.leg == leg) {
                    let turn = seg.turn.expect("a tdrz model must number every turn");
                    assert!(turn >= 1, "turns are 1-based, got {turn}");
                    if let Some(prev) = last {
                        assert!(
                            turn >= prev,
                            "{leg} turns went backwards: {prev} then {turn}"
                        );
                    }
                    last = Some(turn);
                }
            }
        }
    }
```

The monotonicity check is deliberately **per leg**. `process_job` always transcribes both legs and sorts the combined result by `start_secs`, so even a fixture whose `base.en` transcript showed only one leg can yield a stray segment on the other — and `mic` and `system` number independently, making an interleaved `mic/T4` → `system/T1` correct rather than a failure.

- [ ] **Step 2: Confirm it is skipped by default**

Run: `cargo test fixture_chunk`
Expected: `1 ignored`, and the ordinary suite still passes.

- [ ] **Step 3: Run it for real**

Run: `cargo test --release fixture_chunk -- --ignored --nocapture`
Expected: PASS, with two labelled transcripts printed.

- [ ] **Step 4: Judge the output by eye**

Read the printed transcripts and confirm:
- turn numbers advance at plausible speaker changes, not mid-sentence
- `chunk-139` (system, remote multi-speaker) shows more than one turn
- text quality is at least as good as the `base.en` baseline in `fixtures/2026-08-12T15-53-14/chunk-139.json`

If turn boundaries are obviously wrong — every segment its own turn, or one turn for the whole chunk — **stop and report** rather than proceeding. That is the risk the spec flagged, and the retreat is `MEETRS_MODEL` back to `base.en`.

- [ ] **Step 5: Confirm transcription still outpaces recording**

Run: `just run`, speak for ~2 minutes across a couple of pauses, then quit with `q`.
Expected: `TranscribeBacklog` stays at 0-1 rather than climbing, `TranscribeDone`'s `took` is well under the chunk's `audio` duration, and the live TUI lines carry `/T` labels. Confirm the written `transcript.md` and `chunk-NNN.json` agree with what the TUI showed.

- [ ] **Step 6: Commit**

```bash
git add src/transcribe.rs
git commit -m "test(transcribe): add ignored fixture check for turn labels"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| Data model — `turn: Option<u32>` | 2 (step 3) |
| Turn assignment + both subtleties | 2 (steps 1, 4) |
| Split for testability (`RawSegment` / `number_turns`) | 2 |
| `speaker_label` shared by all sinks | 1, wired in 2 (step 8) and 3 (step 8) |
| `chunk-NNN.json` additive field | 2 (step 3, via `Serialize`) |
| `transcript.md` + TUI lines | 2 (step 8) |
| `segments` table + `user_version = 2` | 3 (steps 3, 4) |
| `just search` rendering | 3 (step 8) |
| `--reindex` / `#[serde(default)]` | 3 (step 5) |
| FTS + triggers untouched | 3 (step 3 comment); no trigger edits anywhere |
| Model swap constants | 4 (step 7) |
| `set_tdrz_enable(true)` | 4 (step 5) |
| Orphaned old model → README only | 4 (step 8) |
| Non-tdrz degradation + one warning per session | 4 (steps 3, 6) |
| All seven `number_turns` / `speaker_label` unit tests | 1, 2 |
| Three DB tests | 3 (step 1, plus the updated version assertions in step 6) |
| Fixture-based acceptance | 5 |
| Backlog stays flat under `small.en` | 5 (step 5) |

No gaps found.

**Placeholder scan:** No TBD/TODO. Every code step carries real code. The one deliberate temporary value — `false` at the `process_job` call in Task 2 step 8 — is labelled as such and replaced in Task 4 step 6.

**Type consistency:** `number_turns(Vec<RawSegment>, &'static str, Duration, bool) -> Vec<SegmentOut>` and `speaker_label(&str, Option<u32>) -> String` are used with those exact signatures everywhere. `turn` is `Option<u32>` in `SegmentOut`, `SegmentIn`, `Hit`, and `RebuildSegment`; SQLite stores it via `params!` (which maps `None` to NULL) and reads it back with `r.get::<_, Option<u32>>`. `speaker_label` takes `&str`, so it accepts both `SegmentOut.leg` (`&'static str`) and `Hit.leg` (`String`, via `&h.leg`).

**Two pre-existing tests this plan knowingly edits**, both in Task 3 step 6 — `migration_runs_once_and_is_idempotent`'s two version assertions, and the `seg` helper. Nothing else existing is touched.
