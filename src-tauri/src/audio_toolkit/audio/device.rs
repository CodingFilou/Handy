use cpal::traits::{DeviceTrait, HostTrait};

pub struct CpalDeviceInfo {
    pub index: String,
    pub name: String,
    pub is_default: bool,
    pub device: cpal::Device,
}

pub fn list_input_devices() -> Result<Vec<CpalDeviceInfo>, Box<dyn std::error::Error>> {
    let host = crate::audio_toolkit::get_cpal_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());

    let mut out = Vec::<CpalDeviceInfo>::new();

    for (index, device) in host.input_devices()?.enumerate() {
        let name = device.name().unwrap_or_else(|_| "Unknown".into());

        let is_default = Some(name.clone()) == default_name;

        out.push(CpalDeviceInfo {
            index: index.to_string(),
            name,
            is_default,
            device,
        });
    }

    Ok(out)
}

pub fn list_output_devices() -> Result<Vec<CpalDeviceInfo>, Box<dyn std::error::Error>> {
    let host = crate::audio_toolkit::get_cpal_host();
    let default_name = host.default_output_device().and_then(|d| d.name().ok());

    let mut out = Vec::<CpalDeviceInfo>::new();

    for (index, device) in host.output_devices()?.enumerate() {
        let name = device.name().unwrap_or_else(|_| "Unknown".into());

        let is_default = Some(name.clone()) == default_name;

        out.push(CpalDeviceInfo {
            index: index.to_string(),
            name,
            is_default,
            device,
        });
    }

    Ok(out)
}

/// Loopback capture candidates: the devices whose *output* can be recorded
/// as system audio. On Windows these back WASAPI loopback streams (see
/// `audio::loopback`); on other platforms the list still lets users pick a
/// non-default output to capture once platform support lands.
pub fn list_system_devices() -> Result<Vec<CpalDeviceInfo>, Box<dyn std::error::Error>> {
    list_output_devices()
}

/// Whether this platform can capture system output without extra setup.
/// Windows uses native WASAPI loopback. Linux exposes PulseAudio/PipeWire
/// "Monitor of …" sources as *input* devices on many setups — those are
/// picked via the microphone selector instead. macOS needs a virtual audio
/// driver (e.g. BlackHole) or ScreenCaptureKit (not yet implemented).
pub fn system_capture_supported() -> bool {
    cfg!(target_os = "windows")
}
