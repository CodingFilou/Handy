//! Meeting transcripts: speaker attribution and file output.
//!
//! Dependency-free on purpose (pure functions only) so the logic stays
//! unit-testable without a Tauri handle:
//!
//! * [`assign_speakers`] labels timed transcript segments by comparing the
//!   per-channel energy of the microphone and system-audio captures. In
//!   `Both` mode the two recorders run side by side, so the louder channel
//!   in a segment window is a strong speaker signal — no ML model, no
//!   language dependence, fully offline. (True multi-speaker diarization
//!   *within* one channel — Sprecher C, D, … — needs an embedding model and
//!   is tracked separately; this covers the meeting standard case
//!   "ich vs. alle anderen".)
//! * [`format_meeting_markdown`] renders the labeled transcript.
//! * [`meeting_file_name`] builds the timestamped file name.

/// Sample rate of all capture buffers (16 kHz mono).
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per millisecond at [`SAMPLE_RATE`].
const SAMPLES_PER_MS: i64 = (SAMPLE_RATE / 1000) as i64;

/// One timed transcript segment (e.g. a whisper segment row).
#[derive(Debug, Clone, PartialEq)]
pub struct TimedSegment {
    pub t0_ms: i64,
    pub t1_ms: i64,
    pub text: String,
}

/// A segment with an attributed speaker display name.
#[derive(Debug, Clone, PartialEq)]
pub struct LabeledSegment {
    pub speaker: String,
    pub t0_ms: i64,
    pub t1_ms: i64,
    pub text: String,
}

/// Which capture channels were available for a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelPresence {
    /// Microphone + system audio captured side by side: per-segment labels.
    Both,
    /// Only the microphone captured: everything is the local speaker.
    MicOnly,
    /// Only system audio captured: everything is the remote side.
    SystemOnly,
}

/// Energy ratio (louder/quieter) above which a channel clearly wins a
/// segment. Below it both channels carry the sound (echo, crosstalk) and
/// the louder one still wins — ties are broken towards the microphone.
const WIN_RATIO: f32 = 2.0;
/// RMS below this counts as digital silence; a fully silent window keeps
/// the previous speaker instead of flipping on noise.
const SILENCE_RMS: f32 = 1e-4;

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

fn window(buf: &[f32], t0_ms: i64, t1_ms: i64) -> &[f32] {
    if buf.is_empty() || t1_ms <= t0_ms {
        return &[];
    }
    let start = (t0_ms.max(0) * SAMPLES_PER_MS) as usize;
    let end = (t1_ms.max(0) * SAMPLES_PER_MS) as usize;
    if start >= buf.len() {
        return &[];
    }
    &buf[start..end.min(buf.len())]
}

/// Attribute each segment to the microphone or system speaker.
///
/// `mic`/`sys` are the raw per-channel 16 kHz mono buffers (unmixed, same
/// clock within a few hundred ms — capture starts them back to back).
/// Segments usually come from the mixed transcript, so times may exceed the
/// shorter buffer; [`window`] clamps gracefully.
///
/// Fallback behavior keeps every segment labeled: with a single channel the
/// whole transcript gets that channel's name; with no channel audio at all
/// segments keep the microphone name (historic single-source behavior).
pub fn assign_speakers(
    segments: &[TimedSegment],
    mic: Option<&[f32]>,
    sys: Option<&[f32]>,
    mic_name: &str,
    sys_name: &str,
) -> Vec<LabeledSegment> {
    let presence = match (mic, sys) {
        (Some(_), Some(_)) => ChannelPresence::Both,
        (Some(_), None) => ChannelPresence::MicOnly,
        (None, Some(_)) => ChannelPresence::SystemOnly,
        (None, None) => ChannelPresence::MicOnly,
    };
    let mut out = Vec::with_capacity(segments.len());
    let mut current = match presence {
        ChannelPresence::SystemOnly => sys_name.to_string(),
        _ => mic_name.to_string(),
    };
    for seg in segments {
        if presence == ChannelPresence::Both {
            let mic_rms = rms(window(mic.unwrap_or(&[]), seg.t0_ms, seg.t1_ms));
            let sys_rms = rms(window(sys.unwrap_or(&[]), seg.t0_ms, seg.t1_ms));
            if mic_rms >= SILENCE_RMS || sys_rms >= SILENCE_RMS {
                if mic_rms >= sys_rms * WIN_RATIO {
                    current = mic_name.to_string();
                } else if sys_rms >= mic_rms * WIN_RATIO {
                    current = sys_name.to_string();
                } else if mic_rms >= sys_rms {
                    // Close call (echo/crosstalk): keep it local.
                    current = mic_name.to_string();
                } else {
                    current = sys_name.to_string();
                }
            }
            // Fully silent window: keep the previous speaker (no flip-flop).
        }
        out.push(LabeledSegment {
            speaker: current.clone(),
            t0_ms: seg.t0_ms,
            t1_ms: seg.t1_ms,
            text: seg.text.clone(),
        });
    }
    out
}

/// `mm:ss` for segment headers (hours included when >= 1 h).
pub fn format_timestamp(total_ms: i64) -> String {
    let total_s = (total_ms.max(0) / 1000) as u64;
    let h = total_s / 3600;
    let m = (total_s % 3600) / 60;
    let s = total_s % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// File stem for a meeting starting at `now_local`:
/// `Meeting_2026-10-08_14-30-05`. Sortable, no characters that bother
/// Explorer/OneDrive (no colons), second resolution against collisions.
pub fn meeting_file_stem(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> String {
    format!("Meeting_{year:04}-{month:02}-{day:02}_{hour:02}-{minute:02}-{second:02}")
}

/// Plain-text rendering for paste and history: one block per speaker turn,
/// no markdown (target text fields are plain text).
/// Consecutive segments of the same speaker are merged, like in markdown.
pub fn format_meeting_text(segments: &[LabeledSegment]) -> String {
    let mut out = String::new();
    let mut current_speaker: Option<&str> = None;
    for seg in segments {
        if current_speaker != Some(seg.speaker.as_str()) {
            current_speaker = Some(seg.speaker.as_str());
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "{} [{}]:\n",
                seg.speaker,
                format_timestamp(seg.t0_ms)
            ));
        }
        out.push_str(seg.text.trim());
        out.push(' ');
    }
    out.push('\n');
    out
}

/// Render the full meeting file (markdown): header + one line per segment.
/// Consecutive segments of the same speaker are merged into one block so
/// long monologues stay readable.
pub fn format_meeting_markdown(
    title: &str,
    date_line: &str,
    duration_line: &str,
    model_line: &str,
    source_line: &str,
    segments: &[LabeledSegment],
) -> String {
    let mut md = format!(
        "# {title}\n\n- Datum: {date_line}\n- Dauer: {duration_line}\n- Modell: {model_line}\n- Quelle: {source_line}\n\n---\n"
    );
    let mut current_speaker: Option<&str> = None;
    for seg in segments {
        if current_speaker != Some(seg.speaker.as_str()) {
            current_speaker = Some(seg.speaker.as_str());
            md.push_str(&format!(
                "\n**{}** [{}]:\n",
                seg.speaker,
                format_timestamp(seg.t0_ms)
            ));
        }
        md.push_str(seg.text.trim());
        md.push(' ');
    }
    md.push('\n');
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq_hz: f32, len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| (i as f32 * freq_hz * std::f32::consts::TAU / 16000.0).sin() * amp)
            .collect()
    }

    fn seg(t0_ms: i64, t1_ms: i64, text: &str) -> TimedSegment {
        TimedSegment {
            t0_ms,
            t1_ms,
            text: text.to_string(),
        }
    }

    #[test]
    fn louder_channel_wins_each_segment() {
        // 2 s of audio: first second sys-loud, second second mic-loud.
        let mic: Vec<f32> = (0..32_000)
            .map(|i| {
                let amp = if i < 16_000 { 0.05 } else { 0.5 };
                (i as f32 * 440.0 * std::f32::consts::TAU / 16000.0).sin() * amp
            })
            .collect();
        let sys: Vec<f32> = (0..32_000)
            .map(|i| {
                let amp = if i < 16_000 { 0.5 } else { 0.05 };
                (i as f32 * 440.0 * std::f32::consts::TAU / 16000.0).sin() * amp
            })
            .collect();
        let labeled = assign_speakers(
            &[seg(0, 1000, "hello"), seg(1000, 2000, "world")],
            Some(&mic),
            Some(&sys),
            "A",
            "B",
        );
        assert_eq!(labeled[0].speaker, "B");
        assert_eq!(labeled[1].speaker, "A");
    }

    #[test]
    fn single_channel_labels_everything() {
        let mic = tone(440.0, 16_000, 0.3);
        let labeled = assign_speakers(&[seg(0, 1000, "solo")], Some(&mic), None, "A", "B");
        assert_eq!(labeled.len(), 1);
        assert_eq!(labeled[0].speaker, "A");
    }

    #[test]
    fn silent_window_keeps_previous_speaker() {
        let mic = vec![0.0f32; 32_000];
        let mut sys = vec![0.0f32; 32_000];
        for (i, s) in tone(440.0, 16_000, 0.4).into_iter().enumerate() {
            sys[i] = s;
        }
        let labeled = assign_speakers(
            &[seg(0, 1000, "remote"), seg(1000, 2000, "...")],
            Some(&mic),
            Some(&sys),
            "A",
            "B",
        );
        assert_eq!(labeled[0].speaker, "B");
        assert_eq!(labeled[1].speaker, "B");
    }

    #[test]
    fn markdown_merges_same_speaker_blocks() {
        let md = format_meeting_markdown(
            "Meeting",
            "08.10.2026 14:30",
            "2 min",
            "whisper",
            "Beide",
            &[
                LabeledSegment {
                    speaker: "A".into(),
                    t0_ms: 0,
                    t1_ms: 1000,
                    text: "eins".into(),
                },
                LabeledSegment {
                    speaker: "A".into(),
                    t0_ms: 1000,
                    t1_ms: 2000,
                    text: "zwei".into(),
                },
                LabeledSegment {
                    speaker: "B".into(),
                    t0_ms: 2000,
                    t1_ms: 3000,
                    text: "drei".into(),
                },
            ],
        );
        assert_eq!(md.matches("**A**").count(), 1);
        assert!(md.contains("eins zwei"));
        assert!(md.contains("**B** [00:02]"));
    }

    #[test]
    fn file_stem_format() {
        assert_eq!(
            meeting_file_stem(2026, 10, 8, 14, 30, 5),
            "Meeting_2026-10-08_14-30-05"
        );
    }

    #[test]
    fn plain_text_format() {
        let txt = format_meeting_text(&[
            LabeledSegment {
                speaker: "A".into(),
                t0_ms: 0,
                t1_ms: 1000,
                text: "eins".into(),
            },
            LabeledSegment {
                speaker: "B".into(),
                t0_ms: 61_000,
                t1_ms: 62_000,
                text: "zwei".into(),
            },
        ]);
        assert!(txt.starts_with("A [00:00]:\neins "));
        assert!(txt.contains("\nB [01:01]:\nzwei "));
        assert!(!txt.contains("**"));
    }

    #[test]
    fn timestamp_format() {
        assert_eq!(format_timestamp(65_000), "01:05");
        assert_eq!(format_timestamp(3_661_000), "01:01:01");
    }
}
