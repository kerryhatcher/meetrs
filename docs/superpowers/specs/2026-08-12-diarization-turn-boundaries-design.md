# Speaker-turn boundaries (Tier 1 diarization)

Date: 2026-08-12
Status: approved, not yet implemented

## Goal

Mark where the speaker changes, within each audio leg, using whisper.cpp's
tinydiarize support. Ship this alone, before any embedding-based work.

## Why this and not full diarization

Diarization splits into three problems of very different cost:

1. **Turn boundaries** — where did the speaker change? tinydiarize answers this
   with a model swap and roughly twenty lines of code.
2. **Speaker separation** — which turns belong to the same person? Needs speaker
   embeddings (ECAPA-TDNN / WeSpeaker x-vectors) plus clustering, which means an
   ONNX runtime. That contradicts the deliberate choices recorded in `Cargo.toml`
   — `earshot` was picked specifically to avoid ONNX.
3. **Identification** — what is that person's name? Needs enrolled voice profiles
   or inference from calendar attendees plus the transcript itself.

Only (1) is cheap. It is also a prerequisite for (2): the turn boundaries become
the segmentation input that clustering consumes. So it is worth building first
even on the assumption that (2) and (3) follow.

### The idea this design rejects

An earlier framing assumed the mic leg is "me" and the system leg is "everyone
else", making two-speaker diarization free. **That is wrong.** The leg identifies
the *acoustic path*, not the speaker. In an in-person meeting there is no system
audio at all and every participant arrives on the mic leg. Leg-based labelling is
therefore incorrect in exactly the case where diarization matters most, and no
part of this design relies on it.

## Non-goals

- Speaker identity. Nothing here says two turns are the same person.
- Names. No enrollment, no calendar integration.
- Session-global turn numbering. Numbers are chunk-local by construction.
- Backfilling existing sessions. Old rows keep a NULL turn.
- Grooming the superseded model file off disk.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Numbering scheme | Monotonic `T1, T2, T3…` | Asserts only what tdrz knows: adjacent turns differ. Alternating `S1/S2` would fabricate a two-speaker conversation. |
| Scope | Both legs | Identical code path, and the mic leg is the only diarization available for in-person meetings. |
| Model | Replace the default outright | Keeps `model.rs` single-model; `small.en` also has lower WER than the current quantized `base.en`, so text quality improves too. |
| Numbering scope | Per chunk, per leg | Counters reset each `transcribe_leg` call. Cross-chunk identity is Tier 2's job. |
| Unknown turn | `Option<u32>` = `None` | One representation covers pre-existing DB rows and non-tdrz models. |

### Why monotonic numbering matters

`next_segment_speaker_turn()` on segment *i* predicts a change **after** *i*. That
establishes turn *N* differs from turn *N+1*. It says nothing about whether turn 3
returns to turn 1's speaker. Alternating between two labels would assert exactly
that, and in a five-person meeting the assertion is false — a confident
misattribution that would propagate into any downstream summary. Monotonic
numbering makes the weaker, true claim.

## Feasibility, verified 2026-08-12

- `WhisperSegment::next_segment_speaker_turn()` exists at
  `whisper-rs-0.16.0/src/whisper_state/segment.rs:83`, a safe wrapper over
  `whisper_full_get_segment_speaker_turn_next_from_state`. No fork, no patch, no
  `unsafe` in our code.
- `FullParams::set_tdrz_enable()` exists at
  `whisper-rs-0.16.0/src/whisper_params.rs:262`.
- The model is **not** in `ggerganov/whisper.cpp` (404). It is at
  `akashmjn/tinydiarize-whisper.cpp`, uploaded 2023-06-19:
  - `ggml-small.en-tdrz.bin`
  - 487,614,184 bytes
  - sha256 `ceac3ec06d1d98ef71aec665283564631055fd6129b79d8e1be4f9cc33cc54b4`
    (git-lfs `oid` from the HuggingFace API, same source the existing constant
    was verified against)
  - No quantized variant published; `.q5_1` returns 404.
- Current model for comparison: `ggml-base.en-q5_1.bin`, 59,721,011 bytes.
- `tdrz_enable` only stops whisper.cpp suppressing token 50359 (`[_SOLM_]`) at
  `-INFINITY` (`whisper.cpp:6202`). There is no API to ask a model whether it was
  trained to emit that token.

## Design

### Data model

`SegmentOut` gains one field:

```rust
turn: Option<u32>,   // chunk-local, per-leg, 1-based; None = unknown
```

`None` means "no turn information available", which arises two ways: a segment
recorded before this feature existed, or a model that is not tdrz-capable.

### Turn assignment

`transcribe_leg` splits into two pieces so the interesting logic is testable
without constructing a `WhisperSegment` (which is not possible outside
whisper-rs):

1. A thin collection loop over whisper's segments, producing
   `RawSegment { text, start_cs, end_cs, no_speech_prob, turn_flag }`.
2. A pure `number_turns(raws, leg, offset, tdrz_available) -> Vec<SegmentOut>`
   that filters, numbers, and converts chunk-local centisecond timestamps to
   session-relative seconds via the existing `session_relative_secs`.

This matches how `downmix_leg`, `resample_to_whisper_rate`,
`is_non_speech_marker`, and `session_relative_secs` are already factored in
`transcribe.rs`.

The numbering rule:

```rust
let mut turn = 1;
for raw in raws {
    if !is_non_speech_marker(&raw.text) {
        out.push(SegmentOut { turn: tdrz_available.then_some(turn), .. });
    }
    if raw.turn_flag { turn += 1; }   // after assigning, and outside the filter
}
```

Two properties this gets right, both of which are silent bugs if done naively:

- **Filtered segments still advance the counter.** `transcribe_leg` drops
  `[BLANK_AUDIO]`-style segments. A boundary flagged on a dropped segment is
  still real — a pause between two speakers is precisely where a non-speech
  marker and a turn change coincide. Incrementing inside the `if` would swallow
  the boundary and merge two speakers under one number.
- **A flag on the final segment is a no-op.** The counter reaches a value nothing
  is assigned. No phantom trailing turn appears.

Consecutive segments within one turn share a number, since whisper routinely
splits one speaker's stretch across several timestamp-bounded segments.

### Rendering

One helper, used by every sink so they cannot drift:

```rust
fn speaker_label(leg: &str, turn: Option<u32>) -> String
// Some(3) -> "system/T3"
// None    -> "system"
```

The existing invariant in `transcribe.rs` — "the markdown file and the live UI
must never disagree" — extends to all five sinks:

| Sink | Change |
|---|---|
| `chunk-NNN.json` | additive `"turn": 1` per segment |
| `transcript.md` | `**[00:04:12] system/T1:** text` |
| TUI live lines | `[00:04:12] system/T1: text` |
| `segments` table | new nullable `turn INTEGER` |
| `just search` | `Hit` gains `turn`, rendered identically |

### Database

`db.rs` already migrates on `PRAGMA user_version` (`db.rs:60-140`), so this is a
new `version < 2` block:

```sql
ALTER TABLE segments ADD COLUMN turn INTEGER;
PRAGMA user_version = 2;
```

Nullable deliberately: pre-existing rows read NULL, which is distinguishable from
a real turn number. No backfill — sessions transcribed under `base.en` have no
turn data and cannot acquire it without re-transcribing.

`segments_fts` is external-content FTS5 over `text` alone. A turn number is not
searchable text, so the virtual table and all three triggers are untouched.

For `--reindex`, `RebuildSegment` gains `#[serde(default)] turn: Option<u32>`;
chunk JSONs written before this change lack the field and deserialize to `None`.
`SegmentIn.turn` is `Option<u32>` to match.

### Model swap

`model.rs` keeps its shape. Three constants change, and the `Verified` comment
gets today's date plus the byte count:

```rust
const FILENAME: &str = "ggml-small.en-tdrz.bin";
const URL: &str = "https://huggingface.co/akashmjn/tinydiarize-whisper.cpp/resolve/main/ggml-small.en-tdrz.bin";
const SHA256: &str = "ceac3ec06d1d98ef71aec665283564631055fd6129b79d8e1be4f9cc33cc54b4";
```

`full_params()` gains `params.set_tdrz_enable(true)`, unconditionally. The
existing progress callback already handles content-length and needs no change for
the larger download.

`ensure` looks only for the new `FILENAME`, so the superseded
`ggml-base.en-q5_1.bin` remains on disk as a 57 MiB orphan. Consistent with the
project's stated position that it grooms nothing; note it in the README rather
than writing cleanup code.

### Degradation on a non-tdrz model

`MEETRS_MODEL` must stay a working escape hatch. Pointed at `base.en`,
`tdrz_enable` leaves token 50359 un-suppressed on a model never trained to emit
it: it effectively never fires, so every segment would collapse into `T1` —
misleading noise.

Detection is by filename containing `tdrz`, which is what whisper.cpp's own
tooling keys on and the only signal available. When absent:

- emit a `Status::Warning` that turn detection is unavailable for this model,
  once per session at model load in `run()` — not once per chunk
- pass `tdrz_available = false`, so every `turn` is `None` and labels render bare

This is one boolean threaded into `number_turns` plus the existing warning
channel, not a second pipeline.

## Testing

`number_turns` is pure, so the cases are cheap:

- no flags → every segment turn 1
- flag on segment 0 of 3 → `[1, 2, 2]`
- **flag on a `[BLANK_AUDIO]` segment** → survivors get `[1, 2]`, not `[1, 1]`
  (regression guard for the swallowed-boundary bug)
- flag on the final segment → output length unchanged, no phantom turn
- `tdrz_available = false` → every turn `None`
- empty input → empty output

`speaker_label`: `Some(3)` → `"system/T3"`, `None` → `"system"`.

Database:

- migrating a v1 database adds the column and reports `user_version = 2`
- `Some(2)` round-trips through `record_segments`
- reindexing a turn-less chunk JSON inserts NULL rather than failing to parse

Existing `downmix_leg` and migration-race tests stay untouched.

### Fixtures

`fixtures/2026-08-12T15-53-14/` (gitignored — hundreds of MB of f32 WAV, and
recorded meeting content) holds real captured audio for manual checks, copied
from a live session before compression. All are 3 channels / 48 kHz / f32, with
`mic_channels: [0,0]` and `system_channels: [1,2]`:

| Chunk | Legs | Segments / words | Duration | Case covered |
|---|---|---|---|---|
| `chunk-106` | mic + system | 41 / 264 | 106 s | Densest cross-leg conversation; system leg is the genuine remote multi-speaker case — tdrz's target |
| `chunk-139` | system only | 35 / 266 | 103 s | Single-speaker monologue on the system leg — negative control proving tdrz doesn't invent turns |
| `chunk-209` | mic only | 32 / 235 | 85 s | In-person: everyone on one mic |
| `chunk-114` | mic + system | 25 / 238 | 76 s | Second both-legs sample |
| `chunk-101` | mic + system | 17 / 191 | 69 s | Smaller both-legs sample, fast iteration |

Each `.wav` is accompanied by its original `chunk-NNN.json`, which are `base.en`
transcripts and therefore a before/after baseline for the model swap.
`meta.json` is included because `load_session_meta` requires it to interpret the
channel layout; it is a mid-session snapshot listing 190 chunks, all five of ours
among them.

### Acceptance

Whether tdrz's turn detection is *good* cannot be settled by unit tests. The
check is re-transcribing the fixture chunks and confirming:

- turn numbers advance at real speaker changes and not mid-sentence
- `transcript.md`, the TUI, and `chunk-NNN.json` agree on every label
- `TranscribeBacklog` stays flat under `small.en` — transcription must keep
  outpacing recording, or the chunk pipeline falls behind. On the development
  machine (M2 Pro, 12 cores, Metal) `small.en` unquantized is expected to run
  several times faster than real time, but this is the number to watch.

## Risks

| Risk | Mitigation |
|---|---|
| tinydiarize is a 2023 research prototype; turn quality may disappoint | Fixtures make this measurable before committing further. Fallback is `MEETRS_MODEL` to `base.en`, carried by the degradation path. |
| Model hosted in a single-maintainer HF repo, untouched since 2023 | Checksum is pinned. `MEETRS_MODEL` accepts a manually fetched copy; the download failure message already points at it. |
| Spurious turn splits on a solo mic leg | Cosmetic under monotonic numbering — no identity claim is made. |
| 465 MiB first-run download, 8.2× the current model | One-time, resumable by re-running; progress already reported. |
| T-numbers mistaken downstream for speaker identities | Non-goals state it; `T` rather than `S` signals turn, not speaker. |

## Forward compatibility

Turn boundaries are the segmentation input Tier 2 needs. Clustering collapses
turns into speakers without revising them:

```
Tier 1 (durable):  system/T1  system/T2  system/T3  system/T4
Tier 2 (later):    T1, T3 -> Speaker A ;  T2, T4 -> Speaker B
Tier 3 (later):    Speaker A -> "Dana"
```

Identity therefore arrives as a separate, additional column. The `turn` column
remains segmentation truth and is never rewritten.
