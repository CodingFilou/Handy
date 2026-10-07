// Re-export all audio components
mod device;
pub(crate) mod loopback;
mod recorder;
mod resampler;
mod utils;
mod visualizer;

pub use device::{
    list_input_devices, list_output_devices, list_system_devices, system_capture_supported,
    CpalDeviceInfo,
};
pub use loopback::{mix_mono_16k, LiveMixer, LoopbackRecorder, MixedFrameCallback};
pub use recorder::{
    is_microphone_access_denied, is_no_input_device_error, AudioFrameCallback, AudioRecorder,
    VadPolicy,
};
pub use resampler::FrameResampler;
pub use utils::{read_wav_samples, save_wav_file, verify_wav_file};
pub use visualizer::AudioVisualiser;
