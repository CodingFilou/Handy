use crate::audio_feedback;
#[cfg(not(target_os = "windows"))]
use crate::audio_toolkit::audio::list_system_devices;
use crate::audio_toolkit::audio::{
    list_input_devices, list_output_devices, system_capture_supported, AudioRecorder,
};
use crate::audio_toolkit::{LoopbackRecorder, VadPolicy};
use crate::managers::audio::{AudioRecordingManager, MicrophoneMode};
use crate::settings::{get_settings, write_settings, AudioSource};
use log::warn;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

#[cfg(target_os = "windows")]
use winreg::{
    enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
    RegKey, HKEY,
};

#[derive(Serialize, Type)]
pub struct CustomSounds {
    start: bool,
    stop: bool,
}

fn custom_sound_exists(app: &AppHandle, sound_type: &str) -> bool {
    crate::portable::resolve_app_data(app, &format!("custom_{}.wav", sound_type))
        .is_ok_and(|path| path.exists())
}

#[tauri::command]
#[specta::specta]
pub fn check_custom_sounds(app: AppHandle) -> CustomSounds {
    CustomSounds {
        start: custom_sound_exists(&app, "start"),
        stop: custom_sound_exists(&app, "stop"),
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct AudioDevice {
    pub index: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAccess {
    Allowed,
    Denied,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct WindowsMicrophonePermissionStatus {
    pub supported: bool,
    pub overall_access: PermissionAccess,
    pub device_access: PermissionAccess,
    pub app_access: PermissionAccess,
    pub desktop_app_access: PermissionAccess,
}

#[cfg(target_os = "windows")]
fn read_registry_permission_access(root_hkey: HKEY, path: &str) -> PermissionAccess {
    let root = RegKey::predef(root_hkey);
    let Ok(key) = root.open_subkey(path) else {
        return PermissionAccess::Unknown;
    };

    let Ok(value) = key.get_value::<String, _>("Value") else {
        return PermissionAccess::Unknown;
    };

    match value.to_ascii_lowercase().as_str() {
        "allow" => PermissionAccess::Allowed,
        "deny" => PermissionAccess::Denied,
        _ => PermissionAccess::Unknown,
    }
}

#[cfg(target_os = "windows")]
fn get_windows_microphone_permission_status_impl() -> WindowsMicrophonePermissionStatus {
    const MICROPHONE_PATH: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone";
    const DESKTOP_APPS_PATH: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone\\NonPackaged";

    let device_access = read_registry_permission_access(HKEY_LOCAL_MACHINE, MICROPHONE_PATH);
    let app_access = read_registry_permission_access(HKEY_CURRENT_USER, MICROPHONE_PATH);
    let desktop_app_access = read_registry_permission_access(HKEY_CURRENT_USER, DESKTOP_APPS_PATH);

    // Handy is a desktop app, so the NonPackaged key (desktop_app_access) is
    // the relevant permission scope. The UWP master key (app_access) can be
    // "deny" on systems with debloaters (e.g. O&O ShutUp10) without actually
    // blocking desktop app microphone access.
    let overall_access = if device_access == PermissionAccess::Denied {
        PermissionAccess::Denied
    } else if desktop_app_access == PermissionAccess::Denied {
        PermissionAccess::Denied
    } else if desktop_app_access == PermissionAccess::Allowed {
        PermissionAccess::Allowed
    } else if app_access == PermissionAccess::Denied {
        PermissionAccess::Denied
    } else if device_access == PermissionAccess::Allowed && app_access == PermissionAccess::Allowed
    {
        PermissionAccess::Allowed
    } else {
        PermissionAccess::Unknown
    };

    WindowsMicrophonePermissionStatus {
        supported: true,
        overall_access,
        device_access,
        app_access,
        desktop_app_access,
    }
}

#[tauri::command]
#[specta::specta]
pub fn get_windows_microphone_permission_status() -> WindowsMicrophonePermissionStatus {
    #[cfg(target_os = "windows")]
    {
        get_windows_microphone_permission_status_impl()
    }

    #[cfg(not(target_os = "windows"))]
    {
        WindowsMicrophonePermissionStatus {
            supported: false,
            overall_access: PermissionAccess::Unknown,
            device_access: PermissionAccess::Unknown,
            app_access: PermissionAccess::Unknown,
            desktop_app_access: PermissionAccess::Unknown,
        }
    }
}

#[tauri::command]
#[specta::specta]
pub fn open_microphone_privacy_settings() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        Command::new("cmd")
            .args(["/C", "start", "", "ms-settings:privacy-microphone"])
            .spawn()
            .map_err(|e| format!("Failed to open Windows microphone privacy settings: {}", e))?;
        return Ok(());
    }

    #[cfg(not(target_os = "windows"))]
    {
        Err("Opening microphone privacy settings is only supported on Windows".to_string())
    }
}

#[tauri::command]
#[specta::specta]
pub async fn update_microphone_mode(app: AppHandle, always_on: bool) -> Result<(), String> {
    // Update settings (fast, stays inline)
    let mut settings = get_settings(&app);
    settings.always_on_microphone = always_on;
    write_settings(&app, settings);

    // Update the audio manager mode. update_mode can stop/start the cpal stream
    // (blocking CoreAudio) and takes the manager std mutexes — run it on a
    // blocking thread, NOT inline on the webview/main run loop (a slow device
    // open/close would freeze the UI).
    let rm = app.state::<Arc<AudioRecordingManager>>().inner().clone();
    let new_mode = if always_on {
        MicrophoneMode::AlwaysOn
    } else {
        MicrophoneMode::OnDemand
    };

    tokio::task::spawn_blocking(move || rm.update_mode(new_mode))
        .await
        .map_err(|e| format!("audio task join failed: {}", e))?
        .map_err(|e| format!("Failed to update microphone mode: {}", e))
}

#[tauri::command]
#[specta::specta]
pub fn get_microphone_mode(app: AppHandle) -> Result<bool, String> {
    let settings = get_settings(&app);
    Ok(settings.always_on_microphone)
}

#[tauri::command]
#[specta::specta]
pub async fn get_available_microphones() -> Result<Vec<AudioDevice>, String> {
    // cpal device enumeration can stall — run it off the webview/main run loop.
    tokio::task::spawn_blocking(|| {
        let devices =
            list_input_devices().map_err(|e| format!("Failed to list audio devices: {}", e))?;

        let mut result = vec![AudioDevice {
            index: "default".to_string(),
            name: "Default".to_string(),
            is_default: true,
        }];

        result.extend(devices.into_iter().map(|d| AudioDevice {
            index: d.index,
            name: d.name,
            is_default: false, // The explicit default is handled separately
        }));

        Ok::<_, String>(result)
    })
    .await
    .map_err(|e| format!("audio task join failed: {}", e))?
}

#[tauri::command]
#[specta::specta]
pub async fn set_selected_microphone(app: AppHandle, device_name: String) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.selected_microphone = if device_name == "default" {
        None
    } else {
        Some(device_name)
    };
    write_settings(&app, settings);

    // Update the audio manager to use the new device. update_selected_device
    // can restart the cpal stream (blocking CoreAudio) — run it on a blocking
    // thread, not inline on the webview/main run loop.
    let rm = app.state::<Arc<AudioRecordingManager>>().inner().clone();
    tokio::task::spawn_blocking(move || rm.update_selected_device())
        .await
        .map_err(|e| format!("audio task join failed: {}", e))?
        .map_err(|e| format!("Failed to update selected device: {}", e))
}

#[tauri::command]
#[specta::specta]
pub fn get_selected_microphone(app: AppHandle) -> Result<String, String> {
    let settings = get_settings(&app);
    Ok(settings
        .selected_microphone
        .unwrap_or_else(|| "default".to_string()))
}

#[tauri::command]
#[specta::specta]
pub async fn get_available_output_devices() -> Result<Vec<AudioDevice>, String> {
    // cpal device enumeration can stall — run it off the webview/main run loop.
    tokio::task::spawn_blocking(|| {
        let devices =
            list_output_devices().map_err(|e| format!("Failed to list output devices: {}", e))?;

        let mut result = vec![AudioDevice {
            index: "default".to_string(),
            name: "Default".to_string(),
            is_default: true,
        }];

        result.extend(devices.into_iter().map(|d| AudioDevice {
            index: d.index,
            name: d.name,
            is_default: false, // The explicit default is handled separately
        }));

        Ok::<_, String>(result)
    })
    .await
    .map_err(|e| format!("audio task join failed: {}", e))?
}

#[tauri::command]
#[specta::specta]
pub fn set_selected_output_device(app: AppHandle, device_name: String) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.selected_output_device = if device_name == "default" {
        None
    } else {
        Some(device_name)
    };
    write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_selected_output_device(app: AppHandle) -> Result<String, String> {
    let settings = get_settings(&app);
    Ok(settings
        .selected_output_device
        .unwrap_or_else(|| "default".to_string()))
}

#[tauri::command]
#[specta::specta]
pub async fn play_test_sound(app: AppHandle, sound_type: String) {
    let sound = match sound_type.as_str() {
        "start" => audio_feedback::SoundType::Start,
        "stop" => audio_feedback::SoundType::Stop,
        _ => {
            warn!("Unknown sound type: {}", sound_type);
            return;
        }
    };
    audio_feedback::play_test_sound(&app, sound);
}

#[tauri::command]
#[specta::specta]
pub fn set_clamshell_microphone(app: AppHandle, device_name: String) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.clamshell_microphone = if device_name == "default" {
        None
    } else {
        Some(device_name)
    };
    write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_clamshell_microphone(app: AppHandle) -> Result<String, String> {
    let settings = get_settings(&app);
    Ok(settings
        .clamshell_microphone
        .unwrap_or_else(|| "default".to_string()))
}

#[tauri::command]
#[specta::specta]
pub fn is_recording(app: AppHandle) -> bool {
    let audio_manager = app.state::<Arc<AudioRecordingManager>>();
    audio_manager.is_recording()
}

#[tauri::command]
#[specta::specta]
pub async fn get_microphone_channels(device_name: String) -> Result<u16, String> {
    // cpal device enumeration and config queries can stall, so keep them off
    // the webview/main run loop.
    tokio::task::spawn_blocking(move || {
        use cpal::traits::HostTrait;

        let device = if device_name.eq_ignore_ascii_case("default") {
            crate::audio_toolkit::get_cpal_host().default_input_device()
        } else {
            list_input_devices()
                .map_err(|e| format!("Failed to list audio devices: {e}"))?
                .into_iter()
                .find(|device| device.name == device_name)
                .map(|device| device.device)
        };

        match device {
            Some(device) => AudioRecorder::preferred_input_channel_count(&device)
                .map_err(|e| format!("Failed to get microphone config: {e}")),
            None => Ok(1),
        }
    })
    .await
    .map_err(|e| format!("audio task join failed: {e}"))?
}

#[tauri::command]
#[specta::specta]
pub async fn set_selected_channel(app: AppHandle, channel: Option<u16>) -> Result<(), String> {
    // Restarting cpal can block, so keep it off the webview/main run loop. Apply
    // the runtime change before persisting it so a rejected active-recording
    // change does not become effective on the next launch.
    let manager = app.state::<Arc<AudioRecordingManager>>().inner().clone();
    tokio::task::spawn_blocking(move || manager.update_selected_channel(channel))
        .await
        .map_err(|e| format!("audio task join failed: {e}"))?
        .map_err(|e| format!("Failed to update channel selection: {e}"))?;

    let mut settings = get_settings(&app);
    settings.selected_channel = channel;
    write_settings(&app, settings);
    Ok(())
}

/// Which input recordings capture: microphone, system output, or both.
/// Persisted as `audio_source`; pre-feature stores default to `Microphone`.
#[tauri::command]
#[specta::specta]
pub fn get_audio_source(app: AppHandle) -> Result<AudioSource, String> {
    Ok(get_settings(&app).audio_source)
}

#[tauri::command]
#[specta::specta]
pub async fn set_audio_source(app: AppHandle, source: AudioSource) -> Result<(), String> {
    // Apply the runtime change before persisting it so a rejected change
    // (e.g. while recording) does not become effective on the next launch.
    // Restarting capture can block, so keep it off the webview/main run loop.
    let manager = app.state::<Arc<AudioRecordingManager>>().inner().clone();
    tokio::task::spawn_blocking(move || manager.update_audio_source(source))
        .await
        .map_err(|e| format!("audio task join failed: {e}"))?
        .map_err(|e| format!("Failed to update audio source: {e}"))?;

    let mut settings = get_settings(&app);
    settings.audio_source = source;
    write_settings(&app, settings);
    Ok(())
}

/// Whether this OS can capture system output without extra setup (Windows:
/// native WASAPI loopback). The UI uses this to explain alternatives
/// elsewhere (Linux "Monitor of …" input, macOS virtual device).
#[tauri::command]
#[specta::specta]
pub fn is_system_capture_supported() -> bool {
    system_capture_supported()
}

/// Output devices that can be captured as system audio (`None` entry =
/// system default output). Names match the loopback device names, so the
/// selected entry resolves directly in the capture backend.
#[tauri::command]
#[specta::specta]
pub async fn get_available_system_devices() -> Result<Vec<AudioDevice>, String> {
    // Device enumeration can stall — run it off the webview/main run loop.
    tokio::task::spawn_blocking(|| {
        // On Windows the picker lists the WASAPI endpoints directly, so
        // every entry is guaranteed to resolve in the loopback backend.
        // Elsewhere the output-device list doubles as capture candidates.
        #[cfg(target_os = "windows")]
        {
            let devices = crate::audio_toolkit::audio::loopback::wasapi::list_loopback_devices()
                .map_err(|e| format!("Failed to list system devices: {}", e))?;

            let mut result = vec![AudioDevice {
                index: "default".to_string(),
                name: "Default".to_string(),
                is_default: true,
            }];

            result.extend(devices.into_iter().enumerate().map(|(i, d)| AudioDevice {
                index: i.to_string(),
                name: d.name,
                is_default: d.is_default,
            }));

            Ok::<_, String>(result)
        }

        #[cfg(not(target_os = "windows"))]
        {
            let devices = list_system_devices()
                .map_err(|e| format!("Failed to list system devices: {}", e))?;

            let mut result = vec![AudioDevice {
                index: "default".to_string(),
                name: "Default".to_string(),
                is_default: true,
            }];

            result.extend(devices.into_iter().map(|d| AudioDevice {
                index: d.index,
                name: d.name,
                is_default: false, // The explicit default is handled separately
            }));

            Ok::<_, String>(result)
        }
    })
    .await
    .map_err(|e| format!("audio task join failed: {e}"))?
}

#[tauri::command]
#[specta::specta]
pub async fn set_selected_system_device(app: AppHandle, device_name: String) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.selected_system_device = if device_name == "default" {
        None
    } else {
        Some(device_name)
    };
    write_settings(&app, settings);

    // Re-resolve the loopback device. Restarting capture can block — keep it
    // off the webview/main run loop.
    let rm = app.state::<Arc<AudioRecordingManager>>().inner().clone();
    tokio::task::spawn_blocking(move || rm.update_selected_system_device())
        .await
        .map_err(|e| format!("audio task join failed: {e}"))?
        .map_err(|e| format!("Failed to update system device: {e}"))
}

#[tauri::command]
#[specta::specta]
pub fn get_selected_system_device(app: AppHandle) -> Result<String, String> {
    let settings = get_settings(&app);
    Ok(settings
        .selected_system_device
        .unwrap_or_else(|| "default".to_string()))
}

/// One-shot self-test result for system-audio capture.
#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct SystemCaptureTest {
    /// Capture ran and returned audio (regardless of level).
    pub ok: bool,
    /// RMS level of the captured snippet (0.0 = digital silence).
    pub rms: f32,
    /// Peak absolute sample of the captured snippet.
    pub peak: f32,
    /// Captured seconds (16 kHz mono).
    pub seconds: f32,
    /// Human-readable outcome, already naming the failing step on error.
    pub message: String,
}

/// Records ~1.5 s from the configured system-audio device and reports the
/// level. Lets users verify loopback capture in settings (ideally while
/// something plays) without starting a real recording — and surfaces the
/// exact backend error when capture fails. Needs no model: VAD is bypassed.
#[tauri::command]
#[specta::specta]
pub async fn test_system_capture(app: AppHandle) -> Result<SystemCaptureTest, String> {
    let device_name = get_settings(&app).selected_system_device;
    tokio::task::spawn_blocking(move || {
        let mut recorder =
            LoopbackRecorder::new().map_err(|e| format!("Failed to prepare capture: {e}"))?;
        recorder
            .open(device_name.clone())
            .map_err(|e| format!("Failed to open system audio ({what}): {e}", what = device_name.as_deref().unwrap_or("default output")))?;
        let _ready = recorder
            .start(VadPolicy::Disabled)
            .map_err(|e| format!("Failed to start system-audio capture: {e}"))?;
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let samples = recorder
            .stop()
            .map_err(|e| format!("Failed to finish system-audio capture: {e}"))?;
        let _ = recorder.close();

        let n = samples.len();
        let (rms, peak) = if n == 0 {
            (0.0, 0.0)
        } else {
            let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
            (
                (sum_sq / n as f64).sqrt() as f32,
                samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max),
            )
        };
        let seconds = n as f32 / 16000.0;
        let message = if n == 0 {
            "Capture ran but returned no audio. Is something playing on the selected output?"
                .to_string()
        } else if rms < 0.005 {
            format!(
                "System audio works, but it is (near) silent ({seconds:.1}s captured). Play something and test again."
            )
        } else {
            format!("System audio works ({seconds:.1}s captured, level {rms:.3} RMS).")
        };
        Ok::<_, String>(SystemCaptureTest {
            ok: true,
            rms,
            peak,
            seconds,
            message,
        })
    })
    .await
    .map_err(|e| format!("audio task join failed: {e}"))?
}
