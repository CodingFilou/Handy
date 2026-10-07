//! System-audio (computer sound) capture for meeting transcription.
//!
//! Microphone input covers the user's own voice; this module covers everything
//! else the computer plays — remote meeting participants, calls, media. On
//! Windows it uses native WASAPI loopback capture (no extra driver needed).
//! Other platforms return a descriptive error pointing at their native
//! alternative (Linux: pick the "Monitor of …" input as microphone;
//! macOS: virtual audio driver until ScreenCaptureKit support lands).
//!
//! The capture pipeline (ring buffer → resample → VAD → callbacks) is shared
//! with the microphone recorder: [`LoopbackRecorder`] drives the same
//! [`CaptureProcessor`](super::recorder::CaptureProcessor) through a mirrored
//! consumer loop — only the sample producer differs (WASAPI thread instead
//! of a cpal input callback).

use std::{
    io::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

use rtrb::{Consumer, Producer, RingBuffer};

use super::recorder::{
    acknowledge_pause_after_write, AudioFrameCallback, CaptureProcessor, CaptureTransportState,
    ChunkDisposition, LevelCallback, VadConfig, VadPolicy, AUDIO_RING_SECONDS,
    CONSUMER_POLL_INTERVAL, PAUSE_ACK_TIMEOUT,
};
use crate::audio_toolkit::VoiceActivityDetector;

/// Mix two 16 kHz mono buffers into one. Output length is the longer input
/// (the shorter is zero-padded); samples are averaged so the mix can never
/// clip. This is what the `Both` audio source feeds to transcription.
pub fn mix_mono_16k(a: &[f32], b: &[f32]) -> Vec<f32> {
    let len = a.len().max(b.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0.0);
        let y = b.get(i).copied().unwrap_or(0.0);
        out.push((x + y) * 0.5);
    }
    out
}

/// Callback type for mixed live frames (same shape as the recorder callback).
pub type MixedFrameCallback = Arc<dyn Fn(&[f32]) + Send + Sync + 'static>;

/// Pairs microphone and system-audio live frames into mixed frames for the
/// streaming transcription path.
///
/// Both recorders emit fixed-size 16 kHz frames. When both sides speak, the
/// two frames arrive within one frame period of each other and are mixed.
/// When only one side speaks (the common meeting case — VAD gates the silent
/// side so it emits nothing), the speaking side's frame must not wait forever:
/// a pending frame older than `solo_timeout` is emitted on its own. This keeps
/// live transcription latency bounded while staying correct in both cases.
pub struct LiveMixer {
    emit: MixedFrameCallback,
    solo_timeout: Duration,
    mic_pending: Mutex<Option<(Instant, Vec<f32>)>>,
    sys_pending: Mutex<Option<(Instant, Vec<f32>)>>,
}

impl LiveMixer {
    pub fn new(emit: MixedFrameCallback, frame_period: Duration) -> Self {
        // Three frame periods tolerate scheduler jitter without adding
        // noticeable live-transcript lag (~90 ms at 30 ms frames).
        let solo_timeout = frame_period * 3 + Duration::from_millis(10);
        Self {
            emit,
            solo_timeout,
            mic_pending: Mutex::new(None),
            sys_pending: Mutex::new(None),
        }
    }

    pub fn push_mic(&self, frame: &[f32]) {
        self.push(true, frame);
    }

    pub fn push_sys(&self, frame: &[f32]) {
        self.push(false, frame);
    }

    fn push(&self, is_mic: bool, frame: &[f32]) {
        let (mine, theirs) = if is_mic {
            (&self.mic_pending, &self.sys_pending)
        } else {
            (&self.sys_pending, &self.mic_pending)
        };
        // Sequential locking only — never hold both mutexes at once.
        flush_if_stale(mine, self.solo_timeout, &self.emit);
        flush_if_stale(theirs, self.solo_timeout, &self.emit);
        if let Some((_, other)) = theirs.lock().unwrap().take() {
            let mixed = mix_mono_16k(frame, &other);
            (self.emit)(&mixed);
        } else {
            *mine.lock().unwrap() = Some((Instant::now(), frame.to_vec()));
        }
    }

    /// Emit whatever is still pending (solo, or mixed if both sides have a
    /// frame). Called once the recording stopped so the live overlay shows
    /// the tail instead of dropping it.
    pub fn flush(&self) {
        let mic = self.mic_pending.lock().unwrap().take();
        let sys = self.sys_pending.lock().unwrap().take();
        match (mic, sys) {
            (Some((_, m)), Some((_, s))) => (self.emit)(&mix_mono_16k(&m, &s)),
            (Some((_, m)), None) => (self.emit)(&m),
            (None, Some((_, s))) => (self.emit)(&s),
            (None, None) => {}
        }
    }
}

fn flush_if_stale(
    slot: &Mutex<Option<(Instant, Vec<f32>)>>,
    timeout: Duration,
    emit: &MixedFrameCallback,
) {
    let frame = {
        let mut guard = slot.lock().unwrap();
        let stale = matches!(&*guard, Some((at, _)) if at.elapsed() >= timeout);
        if stale {
            guard.take().map(|(_, frame)| frame)
        } else {
            None
        }
    };
    if let Some(frame) = frame {
        emit(&frame);
    }
}

/* ── ring producer shared with the WASAPI thread ─────────────────────── */

/// Write already-mono `f32` samples into the capture ring, honoring the same
/// pause/overrun protocol as the cpal input callback
/// ([`write path`](super::recorder::AudioRecorder)). The WASAPI capture
/// thread calls this; the consumer side is untouched shared code.
fn write_mono_to_ring(
    data: &[f32],
    producer: &mut Producer<f32>,
    transport: &CaptureTransportState,
) {
    use std::sync::atomic::Ordering;

    if transport.pause_requested.load(Ordering::Acquire)
        && transport.pause_acknowledged.load(Ordering::Acquire)
    {
        return;
    }

    let writable = producer.slots().min(data.len());
    if writable > 0 {
        let chunk = producer
            .write_chunk_uninit(writable)
            .expect("the producer just reported this many writable slots");
        chunk.fill_from_iter(data.iter().take(writable).copied());
    }

    let dropped = data.len() - writable;
    if dropped > 0 {
        transport
            .overrun_samples
            .fetch_add(dropped as u64, Ordering::Relaxed);
    }

    acknowledge_pause_after_write(transport);
}

/* ── system-audio recorder ───────────────────────────────────────────── */

enum LoopbackCmd {
    Start(VadPolicy, Instant, mpsc::Sender<()>),
    Stop(mpsc::Sender<Vec<f32>>),
    Shutdown,
}

/// Captures the computer's own output (system audio) through the same
/// resample → VAD → callback pipeline as the microphone recorder.
///
/// The public shape mirrors [`AudioRecorder`](super::recorder::AudioRecorder)
/// so [`AudioRecordingManager`](crate::managers::audio::AudioRecordingManager)
/// can drive both uniformly; only the sample producer differs (WASAPI
/// loopback thread instead of a cpal input stream).
pub struct LoopbackRecorder {
    device_name: Option<String>,
    cmd_tx: Option<mpsc::Sender<LoopbackCmd>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    vad: Option<VadConfig>,
    level_cb: Option<LevelCallback>,
    audio_cb: Option<AudioFrameCallback>,
    stream_error: Arc<AtomicBool>,
}

impl LoopbackRecorder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(LoopbackRecorder {
            device_name: None,
            cmd_tx: None,
            worker_handle: None,
            vad: None,
            level_cb: None,
            audio_cb: None,
            stream_error: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn with_vad(
        mut self,
        detector: Box<dyn VoiceActivityDetector>,
        offline_hangover_frames: usize,
        streaming_hangover_frames: usize,
    ) -> Self {
        self.vad = Some(VadConfig::with_detector(
            detector,
            offline_hangover_frames,
            streaming_hangover_frames,
        ));
        self
    }

    pub fn with_level_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(Vec<f32>) + Send + Sync + 'static,
    {
        self.level_cb = Some(Arc::new(cb));
        self
    }

    pub fn with_audio_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&[f32]) + Send + Sync + 'static,
    {
        self.audio_cb = Some(Arc::new(cb));
        self
    }

    /// Open the loopback stream for `device_name` (`None` = default output).
    /// See the module docs for platform support.
    pub fn open(&mut self, device_name: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
        if self.worker_handle.is_some() {
            if !self.needs_reopen() {
                return Ok(()); // already open
            }
            log::warn!("Capture stream failed; rebuilding system-audio stream");
            self.close()?;
        }

        self.stream_error.store(false, Ordering::Relaxed);

        let (cmd_tx, cmd_rx) = mpsc::channel::<LoopbackCmd>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let thread_device_name = device_name.clone();
        let vad = self.vad.clone();
        let level_cb = self.level_cb.clone();
        let audio_cb = self.audio_cb.clone();
        let stream_error = Arc::clone(&self.stream_error);

        let worker = std::thread::spawn(move || {
            let transport = Arc::new(CaptureTransportState::default());
            let init_result = (|| -> Result<
                (
                    std::thread::JoinHandle<()>,
                    Arc<AtomicBool>,
                    u32,
                    Consumer<f32>,
                ),
                String,
            > {
                let sample_rate = wasapi::loopback_sample_rate(thread_device_name.as_deref())?;
                let ring_capacity = sample_rate as usize * AUDIO_RING_SECONDS;
                let (mut sample_producer, sample_consumer) = RingBuffer::new(ring_capacity);

                // Touch the ring's pages before capture starts (same as the
                // microphone path) to avoid page faults in the producer.
                {
                    let chunk = sample_producer
                        .write_chunk(ring_capacity)
                        .expect("new audio ring has its full capacity available");
                    chunk.commit_all();
                }

                let producer_stop = Arc::new(AtomicBool::new(false));
                let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
                let producer_handle = wasapi::spawn_capture(
                    thread_device_name.clone(),
                    sample_producer,
                    Arc::clone(&transport),
                    Arc::clone(&stream_error),
                    Arc::clone(&producer_stop),
                    ready_tx,
                );

                match ready_rx.recv_timeout(Duration::from_secs(15)) {
                    Ok(Ok(())) => Ok((
                        producer_handle,
                        producer_stop,
                        sample_rate,
                        sample_consumer,
                    )),
                    Ok(Err(e)) => {
                        let _ = producer_handle.join();
                        Err(e)
                    }
                    Err(_) => {
                        producer_stop.store(true, Ordering::Release);
                        let _ = producer_handle.join();
                        Err("System-audio capture thread died during startup".to_string())
                    }
                }
            })();

            match init_result {
                Ok((producer_handle, producer_stop, sample_rate, sample_consumer)) => {
                    let _ = init_tx.send(Ok(()));
                    log::info!(
                        "System-audio loopback running at {sample_rate} Hz{}",
                        thread_device_name
                            .as_deref()
                            .map(|n| format!(" (device '{n}')"))
                            .unwrap_or_default()
                    );
                    let processor =
                        CaptureProcessor::new(sample_rate, vad, level_cb, audio_cb, Instant::now());
                    run_loopback_consumer(
                        processor,
                        sample_consumer,
                        cmd_rx,
                        transport,
                        Arc::clone(&stream_error),
                    );
                    producer_stop.store(true, Ordering::Release);
                    let _ = producer_handle.join();
                }
                Err(error_message) => {
                    log::error!("{error_message}");
                    let _ = init_tx.send(Err(error_message));
                }
            }
        });

        match init_rx.recv() {
            Ok(Ok(())) => {
                self.device_name = device_name;
                self.cmd_tx = Some(cmd_tx);
                self.worker_handle = Some(worker);
                Ok(())
            }
            Ok(Err(error_message)) => {
                let _ = worker.join();
                Err(Box::new(Error::other(error_message)))
            }
            Err(recv_error) => {
                let _ = worker.join();
                Err(Box::new(Error::other(format!(
                    "Failed to initialize system-audio worker: {recv_error}"
                ))))
            }
        }
    }

    pub fn start(
        &self,
        vad_policy: VadPolicy,
    ) -> Result<mpsc::Receiver<()>, Box<dyn std::error::Error>> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| Error::other("System-audio recorder is not open"))?;
        let (ready_tx, ready_rx) = mpsc::channel();
        tx.send(LoopbackCmd::Start(vad_policy, Instant::now(), ready_tx))?;
        Ok(ready_rx)
    }

    pub fn stop(&self) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| Error::other("System-audio recorder is not open"))?;
        let (resp_tx, resp_rx) = mpsc::channel();
        tx.send(LoopbackCmd::Stop(resp_tx))?;
        Ok(resp_rx.recv()?)
    }

    pub fn needs_reopen(&self) -> bool {
        self.stream_error.load(Ordering::Relaxed)
            || self
                .worker_handle
                .as_ref()
                .is_some_and(|handle| handle.is_finished())
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(LoopbackCmd::Shutdown);
        }
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
        self.device_name = None;
        Ok(())
    }
}

/// Consumer loop for the loopback worker. It mirrors the microphone
/// [`run_consumer`](super::recorder::run_consumer) (same pause/drain/stop
/// protocol over the same [`CaptureProcessor`]); only the command channel
/// type differs, so it lives here next to [`LoopbackCmd`].
fn run_loopback_consumer(
    mut processor: CaptureProcessor,
    mut sample_consumer: Consumer<f32>,
    cmd_rx: mpsc::Receiver<LoopbackCmd>,
    transport: Arc<CaptureTransportState>,
    stream_error: Arc<AtomicBool>,
) {
    let mut recording = false;
    let mut stream_error_logged = false;

    loop {
        let mut command = if sample_consumer.slots() > 0 {
            match cmd_rx.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        } else {
            match cmd_rx.recv_timeout(CONSUMER_POLL_INTERVAL) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        };

        loop {
            if let Some(cmd) = command.take() {
                match cmd {
                    LoopbackCmd::Start(policy, sent_at, ready_tx) => {
                        log::debug!(
                            "Loopback Cmd::Start processed {:?} after send",
                            sent_at.elapsed()
                        );
                        transport.overrun_samples.store(0, Ordering::Release);
                        processor.begin_recording(policy, ready_tx);
                        recording = true;
                    }
                    LoopbackCmd::Stop(reply_tx) => {
                        processor
                            .observe_overrun(transport.overrun_samples.swap(0, Ordering::AcqRel));
                        recording = false;
                        processor.cancel_ready_signal();

                        transport.pause_acknowledged.store(false, Ordering::Relaxed);
                        transport.pause_requested.store(true, Ordering::Release);
                        let pause_started = Instant::now();
                        while !transport.pause_acknowledged.load(Ordering::Acquire)
                            && pause_started.elapsed() < PAUSE_ACK_TIMEOUT
                        {
                            let drained =
                                processor.drain(&mut sample_consumer, ChunkDisposition::Capture);
                            if drained == 0 {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }

                        let pause_timed_out = !transport.pause_acknowledged.load(Ordering::Acquire);
                        if pause_timed_out {
                            log::warn!("Timed out waiting for the loopback capture to pause");
                            stream_error.store(true, Ordering::Release);
                        }

                        while processor.drain(&mut sample_consumer, ChunkDisposition::Capture) > 0 {
                        }

                        processor
                            .observe_overrun(transport.overrun_samples.swap(0, Ordering::AcqRel));
                        let samples = processor.finish_recording();
                        if !pause_timed_out {
                            transport.pause_acknowledged.store(false, Ordering::Relaxed);
                            transport.pause_requested.store(false, Ordering::Release);
                        }
                        let _ = reply_tx.send(samples);

                        if pause_timed_out {
                            return;
                        }
                    }
                    LoopbackCmd::Shutdown => {
                        transport.pause_requested.store(true, Ordering::Release);
                        return;
                    }
                }
            }

            command = match cmd_rx.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            };
        }

        let disposition = if recording {
            ChunkDisposition::Capture
        } else {
            ChunkDisposition::Discard
        };
        processor.drain(&mut sample_consumer, disposition);

        let overrun_samples = transport.overrun_samples.swap(0, Ordering::AcqRel);
        if recording {
            processor.observe_overrun(overrun_samples);
        }

        if stream_error.load(Ordering::Acquire) && !stream_error_logged {
            log::error!("System-audio backend reported a stream error; it will be rebuilt");
            stream_error_logged = true;
        }
    }
}

/* ── platform backend ────────────────────────────────────────────────── */

/// OS loopback capture. The module above is platform-agnostic; only this
/// submodule differs. On Windows it drives WASAPI directly through the
/// `windows` crate (already a dependency); elsewhere every entry point
/// returns a descriptive error naming the platform's native alternative.
pub(crate) mod wasapi {
    use super::CaptureTransportState;
    use rtrb::Producer;
    use std::sync::{atomic::AtomicBool, mpsc, Arc};

    const UNSUPPORTED: &str = "System-audio capture is currently supported on Windows (WASAPI loopback). On Linux, choose the 'Monitor of …' input as your microphone; on macOS, route system audio through a virtual device.";

    /// One loopback candidate shown in the system-device picker.
    pub struct LoopbackDeviceEntry {
        pub name: String,
        pub is_default: bool,
    }

    /// Mix-format sample rate of the target render endpoint, without starting
    /// capture. Used to size the capture ring before the producer spawns.
    pub fn loopback_sample_rate(device_name: Option<&str>) -> Result<u32, String> {
        #[cfg(target_os = "windows")]
        {
            return win::with_com(|| win::mix_format_rate_of(device_name));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = device_name;
            Err(UNSUPPORTED.to_string())
        }
    }

    /// Active render endpoints as loopback candidates.
    pub fn list_loopback_devices() -> Result<Vec<LoopbackDeviceEntry>, String> {
        #[cfg(target_os = "windows")]
        {
            return win::with_com(win::list_devices);
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(UNSUPPORTED.to_string())
        }
    }

    /// Spawn the capture thread. It reports `Ok(())` on `ready` once the
    /// loopback stream runs, or `Err` when initialization fails.
    pub fn spawn_capture(
        device_name: Option<String>,
        producer: Producer<f32>,
        transport: Arc<CaptureTransportState>,
        stream_error: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        ready: mpsc::SyncSender<Result<(), String>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            #[cfg(target_os = "windows")]
            {
                win::with_com(|| unsafe {
                    win::capture_main(
                        device_name,
                        producer,
                        &transport,
                        &stream_error,
                        &stop,
                        ready,
                    )
                });
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (producer, transport, stream_error, stop);
                let _ = ready.send(Err(UNSUPPORTED.to_string()));
            }
        })
    }

    #[cfg(target_os = "windows")]
    mod win {
        use super::super::{write_mono_to_ring, CaptureTransportState};
        use super::LoopbackDeviceEntry;
        use rtrb::Producer;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::Duration;
        use windows::core::GUID;
        use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
        use windows::Win32::Media::Audio::{
            eMultimedia, eRender, IAudioCaptureClient, IAudioClient, IMMDevice,
            IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE,
            WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
        };
        use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
        use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
        use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
            COINIT_MULTITHREADED, STGM_READ,
        };
        use windows::Win32::System::Variant::VT_LPWSTR;
        use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

        /// 200 ms device buffer (100 ns units) for the shared-mode loopback
        /// stream. Generous enough that a 5 ms poll can never overrun it.
        const BUFFER_DURATION_100NS: i64 = 2_000_000;
        const POLL_INTERVAL: Duration = Duration::from_millis(5);

        /// Run `f` with COM initialized for this thread (multithreaded).
        pub fn with_com<F, T>(f: F) -> T
        where
            F: FnOnce() -> T,
        {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let result = f();
            unsafe {
                CoUninitialize();
            }
            result
        }

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum SampleKind {
            F32,
            I16,
            I32,
        }

        #[derive(Clone, Copy)]
        struct MixFormat {
            channels: usize,
            rate: u32,
            kind: SampleKind,
        }

        /// KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT trailing bytes.
        const KS_BASE: [u8; 8] = [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71];

        fn guid_wave_tag(guid: &GUID) -> Option<u32> {
            if guid.data2 == 0 && guid.data3 == 0x0010 && guid.data4 == KS_BASE {
                Some(guid.data1) // 1 = PCM, 3 = IEEE float
            } else {
                None
            }
        }

        /// Translate a WASAPI mix format into something convertible. Handles
        /// plain PCM/float as well as WAVE_FORMAT_EXTENSIBLE wrappers around
        /// them (what most render endpoints report).
        ///
        /// NOTE: the WASAPI format structs are `packed(1)` in the `windows`
        /// crate, so every field is copied out with `read_unaligned` —
        /// referencing a field would be undefined behavior.
        unsafe fn read_mix_format(format: *const WAVEFORMATEX) -> Result<MixFormat, String> {
            let channels = std::ptr::addr_of!((*format).nChannels).read_unaligned() as usize;
            let rate = std::ptr::addr_of!((*format).nSamplesPerSec).read_unaligned();
            let mut tag = std::ptr::addr_of!((*format).wFormatTag).read_unaligned() as u32;
            let bits = std::ptr::addr_of!((*format).wBitsPerSample).read_unaligned();
            if channels == 0 || channels > 32 || rate == 0 {
                return Err(format!(
                    "Unsupported system-audio format ({} channels, {} Hz)",
                    channels, rate
                ));
            }
            if tag == WAVE_FORMAT_EXTENSIBLE {
                let cb_size = std::ptr::addr_of!((*format).cbSize).read_unaligned();
                if cb_size < 22 {
                    return Err(
                        "Unsupported system-audio format (truncated extensible header)".to_string(),
                    );
                }
                let subformat: GUID =
                    std::ptr::addr_of!((*(format as *const WAVEFORMATEXTENSIBLE)).SubFormat)
                        .read_unaligned();
                tag = guid_wave_tag(&subformat).ok_or_else(|| {
                    "Unsupported system-audio format (only PCM and float loopback are supported)"
                        .to_string()
                })?;
            }
            // `bits` is the container width. (A 24-in-32 PCM stream decodes as
            // I32 with a negligible scale error — fine for transcription.)
            let kind = match (tag, bits) {
                (t, 32) if t == WAVE_FORMAT_IEEE_FLOAT => SampleKind::F32,
                (t, 16) if t == WAVE_FORMAT_PCM => SampleKind::I16,
                (t, 32) if t == WAVE_FORMAT_PCM => SampleKind::I32,
                _ => {
                    return Err(format!(
                        "Unsupported system-audio format (tag {tag}, {bits}-bit)"
                    ));
                }
            };
            Ok(MixFormat {
                channels,
                rate,
                kind,
            })
        }

        unsafe fn friendly_name(device: &IMMDevice) -> Result<String, String> {
            let store: IPropertyStore = device
                .OpenPropertyStore(STGM_READ)
                .map_err(|e| format!("Failed to read audio device properties: {e}"))?;
            let mut prop: PROPVARIANT = store
                .GetValue(&PKEY_Device_FriendlyName as *const _)
                .map_err(|e| format!("Failed to read audio device name: {e}"))?;
            let name = if prop.Anonymous.Anonymous.vt == VT_LPWSTR {
                prop.Anonymous
                    .Anonymous
                    .Anonymous
                    .pwszVal
                    .to_string()
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let _ = PropVariantClear(&mut prop);
            Ok(name)
        }

        unsafe fn resolve_device(
            enumerator: &IMMDeviceEnumerator,
            wanted: Option<&str>,
        ) -> Result<IMMDevice, String> {
            match wanted {
                None => enumerator
                    .GetDefaultAudioEndpoint(eRender, eMultimedia)
                    .map_err(|_| {
                        "No default audio output found. Is an output device enabled?".to_string()
                    }),
                Some(name) => {
                    let collection = enumerator
                        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                        .map_err(|e| format!("Failed to list audio outputs: {e}"))?;
                    let count = collection
                        .GetCount()
                        .map_err(|e| format!("Failed to count audio outputs: {e}"))?;
                    for i in 0..count {
                        let dev = collection
                            .Item(i)
                            .map_err(|e| format!("Failed to inspect audio output: {e}"))?;
                        let friendly = friendly_name(&dev).unwrap_or_default();
                        // Exact match first (the picker lists these very
                        // names); a case-insensitive fallback tolerates
                        // cpal/WASAPI casing differences.
                        if friendly == name || friendly.eq_ignore_ascii_case(name) {
                            return Ok(dev);
                        }
                    }
                    Err(format!(
                        "System-audio device '{name}' was not found. It may have been unplugged or renamed."
                    ))
                }
            }
        }

        /// Open the render endpoint and read its mix format (no capture).
        unsafe fn open_mix_format(
            enumerator: &IMMDeviceEnumerator,
            wanted: Option<&str>,
        ) -> Result<MixFormat, String> {
            let device = resolve_device(enumerator, wanted)?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("Failed to open system-audio device: {e}"))?;
            let format_ptr = client
                .GetMixFormat()
                .map_err(|e| format!("Failed to read device format: {e}"))?;
            let format = read_mix_format(format_ptr);
            CoTaskMemFree(Some(format_ptr as *const _));
            format
        }

        unsafe fn enumerator() -> Result<IMMDeviceEnumerator, String> {
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("Failed to access Windows audio devices: {e}"))
        }

        pub fn mix_format_rate_of(device_name: Option<&str>) -> Result<u32, String> {
            unsafe {
                let enumerator = enumerator()?;
                Ok(open_mix_format(&enumerator, device_name)?.rate)
            }
        }

        pub fn list_devices() -> Result<Vec<LoopbackDeviceEntry>, String> {
            unsafe {
                let enumerator = enumerator()?;
                let default_name = resolve_device(&enumerator, None)
                    .ok()
                    .and_then(|d| friendly_name(&d).ok());
                let collection = enumerator
                    .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                    .map_err(|e| format!("Failed to list audio outputs: {e}"))?;
                let count = collection
                    .GetCount()
                    .map_err(|e| format!("Failed to count audio outputs: {e}"))?;
                let mut out = Vec::new();
                for i in 0..count {
                    let dev = collection
                        .Item(i)
                        .map_err(|e| format!("Failed to inspect audio output: {e}"))?;
                    let name = friendly_name(&dev).unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    out.push(LoopbackDeviceEntry {
                        is_default: Some(name.clone()) == default_name,
                        name,
                    });
                }
                Ok(out)
            }
        }

        /// Average one interleaved device frame to mono and append.
        unsafe fn convert_append(
            data: *const u8,
            frames: usize,
            format: &MixFormat,
            out: &mut Vec<f32>,
        ) {
            let channels = format.channels;
            out.reserve(frames);
            match format.kind {
                SampleKind::F32 => {
                    let src = std::slice::from_raw_parts(data as *const f32, frames * channels);
                    for f in 0..frames {
                        let mut sample = 0.0f32;
                        for c in 0..channels {
                            sample += src[f * channels + c];
                        }
                        out.push(sample / channels as f32);
                    }
                }
                SampleKind::I16 => {
                    let src = std::slice::from_raw_parts(data as *const i16, frames * channels);
                    for f in 0..frames {
                        let mut sample = 0i32;
                        for c in 0..channels {
                            sample += src[f * channels + c] as i32;
                        }
                        out.push(sample as f32 / (channels as f32 * 32768.0));
                    }
                }
                SampleKind::I32 => {
                    let src = std::slice::from_raw_parts(data as *const i32, frames * channels);
                    for f in 0..frames {
                        let mut sample = 0i64;
                        for c in 0..channels {
                            sample += src[f * channels + c] as i64;
                        }
                        out.push(sample as f32 / (channels as f32 * 2147483648.0));
                    }
                }
            }
        }

        /// Full capture: init the loopback stream, report readiness, then pump
        /// packets into the ring until `stop` is set or a fatal error occurs
        /// (reported via `stream_error` so the manager rebuilds the stream,
        /// exactly like the cpal error path).
        pub unsafe fn capture_main(
            device_name: Option<String>,
            mut producer: Producer<f32>,
            transport: &CaptureTransportState,
            stream_error: &AtomicBool,
            stop: &AtomicBool,
            ready: mpsc::SyncSender<Result<(), String>>,
        ) {
            let fail = |message: String| {
                let _ = ready.send(Err(message));
            };

            let enumerator = match enumerator() {
                Ok(e) => e,
                Err(e) => {
                    fail(e);
                    return;
                }
            };
            let device = match resolve_device(&enumerator, device_name.as_deref()) {
                Ok(d) => d,
                Err(e) => {
                    fail(e);
                    return;
                }
            };
            let client: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
                Ok(c) => c,
                Err(e) => {
                    fail(format!("Failed to open system-audio device: {e}"));
                    return;
                }
            };
            let format_ptr = match client.GetMixFormat() {
                Ok(p) => p,
                Err(e) => {
                    fail(format!("Failed to read device format: {e}"));
                    return;
                }
            };
            let format = match read_mix_format(format_ptr) {
                Ok(f) => f,
                Err(e) => {
                    CoTaskMemFree(Some(format_ptr as *const _));
                    fail(e);
                    return;
                }
            };
            if let Err(e) = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_100NS,
                0,
                format_ptr,
                None,
            ) {
                CoTaskMemFree(Some(format_ptr as *const _));
                fail(format!("Failed to start system-audio capture: {e}"));
                return;
            }
            CoTaskMemFree(Some(format_ptr as *const _));

            let capture: IAudioCaptureClient = match client.GetService() {
                Ok(c) => c,
                Err(e) => {
                    fail(format!("Failed to open capture channel: {e}"));
                    return;
                }
            };
            if let Err(e) = client.Start() {
                fail(format!("Failed to start system-audio capture: {e}"));
                return;
            }
            let _ = ready.send(Ok(()));

            let mut mono = Vec::<f32>::new();
            'capture: loop {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if transport.pause_requested.load(Ordering::Acquire)
                    && transport.pause_acknowledged.load(Ordering::Acquire)
                {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                let packets = match capture.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(e) => {
                        log::error!("System-audio capture failed: {e}");
                        stream_error.store(true, Ordering::Release);
                        break;
                    }
                };
                if packets == 0 {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                for _ in 0..packets {
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    if let Err(e) =
                        capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    {
                        log::error!("System-audio capture failed: {e}");
                        stream_error.store(true, Ordering::Release);
                        break 'capture;
                    }
                    mono.clear();
                    if frames > 0 {
                        if (flags as i32) & AUDCLNT_BUFFERFLAGS_SILENT.0 != 0 || data.is_null() {
                            // Loopback flags silence instead of returning
                            // samples; forward zeros so timing stays intact.
                            mono.resize(frames as usize, 0.0);
                        } else {
                            convert_append(data, frames as usize, &format, &mut mono);
                        }
                        write_mono_to_ring(&mono, &mut producer, transport);
                    }
                    if let Err(e) = capture.ReleaseBuffer(frames) {
                        log::error!("System-audio capture failed: {e}");
                        stream_error.store(true, Ordering::Release);
                        break 'capture;
                    }
                }
            }

            let _ = client.Stop();
            // `client`, `capture`, and `device` release their COM references
            // on drop; COM itself is uninitialized by the `with_com` caller.
        }
    }
}

/* ── unit tests ──────────────────────────────────────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn mix_equal_length_averages() {
        assert_eq!(mix_mono_16k(&[1.0, -1.0], &[0.5, 0.5]), vec![0.75, -0.25]);
    }

    #[test]
    fn mix_pads_shorter_with_silence() {
        assert_eq!(mix_mono_16k(&[1.0, 1.0, 1.0], &[0.0]), vec![0.5, 0.5, 0.5]);
    }

    #[test]
    fn mix_empty_inputs() {
        assert!(mix_mono_16k(&[], &[]).is_empty());
        assert_eq!(mix_mono_16k(&[], &[2.0]), vec![1.0]);
        assert_eq!(mix_mono_16k(&[-2.0], &[]), vec![-1.0]);
    }

    #[test]
    fn mix_never_clips() {
        for sample in mix_mono_16k(&[1.0; 128], &[1.0; 64]).iter() {
            assert!((-1.0..=1.0).contains(sample));
        }
    }

    fn collecting_mixer(frame_period: Duration) -> (LiveMixer, Arc<Mutex<Vec<Vec<f32>>>>) {
        let emitted = Arc::new(Mutex::new(Vec::<Vec<f32>>::new()));
        let sink = Arc::clone(&emitted);
        let mixer = LiveMixer::new(
            Arc::new(move |frame: &[f32]| sink.lock().unwrap().push(frame.to_vec())),
            frame_period,
        );
        (mixer, emitted)
    }

    #[test]
    fn live_mixer_pairs_simultaneous_frames() {
        let (mixer, emitted) = collecting_mixer(Duration::from_millis(30));
        mixer.push_mic(&[1.0, 1.0]);
        // Mic frame waits for its system counterpart — nothing emitted yet.
        assert!(emitted.lock().unwrap().is_empty());
        mixer.push_sys(&[0.0, 0.0]);
        let got = emitted.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], vec![0.5, 0.5]);
    }

    #[test]
    fn live_mixer_emits_solo_after_timeout() {
        let (mixer, emitted) = collecting_mixer(Duration::from_millis(10));
        mixer.push_mic(&[1.0, 1.0]);
        // Only the microphone speaks: after the solo timeout the next frame
        // flushes the stale one on its own instead of waiting forever.
        std::thread::sleep(Duration::from_millis(80));
        mixer.push_mic(&[2.0, 2.0]);
        let got = emitted.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], vec![1.0, 1.0]);
    }

    #[test]
    fn live_mixer_flush_emits_pending_tail() {
        let (mixer, emitted) = collecting_mixer(Duration::from_secs(60));
        mixer.push_sys(&[0.25, -0.25]);
        mixer.flush();
        let got = emitted.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], vec![0.25, -0.25]);
    }

    #[test]
    fn live_mixer_flush_mixes_both_pending() {
        let (mixer, emitted) = collecting_mixer(Duration::from_secs(60));
        mixer.push_mic(&[1.0, 0.0]);
        mixer.push_sys(&[0.0, 1.0]);
        // Paired immediately, so flush has nothing left to do.
        mixer.flush();
        let got = emitted.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], vec![0.5, 0.5]);
    }
}
