pub mod audio;
pub mod constants;
pub mod lang_id;
pub mod meeting;
pub mod text;
pub mod utils;
pub mod vad;

pub use audio::{
    is_microphone_access_denied, is_no_input_device_error, list_input_devices, list_output_devices,
    list_system_devices, mix_mono_16k, read_wav_samples, save_wav_file, system_capture_supported,
    verify_wav_file, AudioFrameCallback, AudioRecorder, CpalDeviceInfo, LiveMixer,
    LoopbackRecorder, VadPolicy,
};
pub use lang_id::detect_output_language;
pub use meeting::{
    assign_speakers, format_meeting_markdown, format_meeting_text, format_timestamp,
    meeting_file_stem, ChannelPresence, LabeledSegment, TimedSegment,
};
pub use text::{
    apply_custom_words, normalize_transcription_output, remove_filler_words, OutputLanguageEvidence,
};
pub use utils::get_cpal_host;
pub use vad::{EarshotVad, SileroVad, VoiceActivityDetector};
