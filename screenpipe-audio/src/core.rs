use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamError;
use log::{debug, error, info, warn};
use screenpipe_core::find_ffmpeg_path;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{fmt, thread};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::AudioInput;

async fn get_macos_version() -> Option<f32> {
    let output = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .await
        .ok()?;
    let version = String::from_utf8(output.stdout).ok()?;
    version.split('.').next()?.parse().ok()
}

#[derive(Clone, Debug, PartialEq)]
pub enum AudioTranscriptionEngine {
    Deepgram,
    WhisperTiny,
    WhisperDistilLargeV3,
}

impl fmt::Display for AudioTranscriptionEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioTranscriptionEngine::Deepgram => write!(f, "Deepgram"),
            AudioTranscriptionEngine::WhisperTiny => write!(f, "WhisperTiny"),
            AudioTranscriptionEngine::WhisperDistilLargeV3 => write!(f, "WhisperLarge"),
        }
    }
}

impl Default for AudioTranscriptionEngine {
    fn default() -> Self {
        AudioTranscriptionEngine::WhisperTiny
    }
}

#[derive(Clone)]
pub struct DeviceControl {
    pub is_running: bool,
    pub is_paused: bool,
}

#[derive(Clone, Eq, PartialEq, Hash, Serialize)]
pub enum DeviceType {
    Input,
    Output,
}

#[derive(Clone, Eq, PartialEq, Hash, Serialize)]
pub struct AudioDevice {
    pub name: String,
    pub device_type: DeviceType,
}

impl AudioDevice {
    pub fn new(name: String, device_type: DeviceType) -> Self {
        AudioDevice { name, device_type }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        if name.trim().is_empty() {
            return Err(anyhow!("Device name cannot be empty"));
        }

        let (name, device_type) = if name.to_lowercase().ends_with("(input)") {
            (
                name.trim_end_matches("(input)").trim().to_string(),
                DeviceType::Input,
            )
        } else if name.to_lowercase().ends_with("(output)") {
            (
                name.trim_end_matches("(output)").trim().to_string(),
                DeviceType::Output,
            )
        } else {
            return Err(anyhow!(
                "Device type (input/output) not specified in the name"
            ));
        };

        Ok(AudioDevice::new(name, device_type))
    }
}

impl fmt::Display for AudioDevice {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} ({})",
            self.name,
            match self.device_type {
                DeviceType::Input => "input",
                DeviceType::Output => "output",
            }
        )
    }
}

pub fn parse_audio_device(name: &str) -> Result<AudioDevice> {
    AudioDevice::from_name(name)
}

async fn get_device_and_config(
    audio_device: &AudioDevice,
) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let host = match audio_device.device_type {
        #[cfg(target_os = "macos")]
        DeviceType::Output => cpal::host_from_id(cpal::HostId::ScreenCaptureKit)?,
        _ => cpal::default_host(),
    };

    info!("device: {:?}", audio_device.to_string());

    let is_output_device = audio_device.device_type == DeviceType::Output;

    let audio_device = if audio_device.to_string() == "default" {
        match audio_device.device_type {
            DeviceType::Input => host.default_input_device(),
            DeviceType::Output => host.default_output_device(),
        }
    } else {
        let mut devices = match audio_device.device_type {
            DeviceType::Input => host.input_devices()?,
            DeviceType::Output => host.output_devices()?,
        };

        #[cfg(target_os = "macos")]
        {
            if audio_device.device_type == DeviceType::Output {
                match get_macos_version().await {
                    Some(version) if version < 15.0 => {
                        if let Ok(screen_capture_host) =
                            cpal::host_from_id(cpal::HostId::ScreenCaptureKit)
                        {
                            devices = screen_capture_host.input_devices()?;
                        }
                    }
                    _ => {
                        warn!("Audio output not supported on macOS 15.0 and above");
                        return Err(anyhow!("Audio output not supported on this macOS version"));
                    }
                }
            }
        }

        devices.find(|x| {
            x.name()
                .map(|y| {
                    y == audio_device
                        .to_string()
                        .replace(" (input)", "")
                        .replace(" (output)", "")
                        .trim()
                })
                .unwrap_or(false)
        })
    }
    .ok_or_else(|| anyhow!("Audio device not found"))?;

    let mut config = audio_device.default_input_config()?;
    // if output device and windows, using output config
    if cfg!(target_os = "windows") && is_output_device {
        config = audio_device.default_output_config()?;
    }
    Ok((audio_device, config))
}

async fn run_ffmpeg(
    mut rx: mpsc::Receiver<Vec<u8>>,
    sample_rate: u32,
    channels: u16,
    output_path: &PathBuf,
    is_running: Arc<AtomicBool>,
    duration: Duration,
) -> Result<()> {
    debug!("Starting FFmpeg process");
    let mut command = Command::new(find_ffmpeg_path().unwrap());
    command
        .args(&[
            "-f",
            "f32le",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            &channels.to_string(),
            "-i",
            "pipe:0",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-f",
            "mp4",
            output_path.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    debug!("FFmpeg command: {:?}", command);

    let mut ffmpeg: tokio::process::Child =
        command.spawn().expect("Failed to spawn FFmpeg process");
    debug!("FFmpeg process spawned");
    let mut stdin = ffmpeg.stdin.take().expect("Failed to open stdin");
    let start_time = std::time::Instant::now();

    while is_running.load(Ordering::Relaxed) {
        tokio::select! {
            Some(data) = rx.recv() => {
                if start_time.elapsed() >= duration {
                    debug!("Duration exceeded, breaking loop");
                    break;
                }
                if let Err(e) = stdin.write_all(&data).await {
                    error!("Failed to write audio data to FFmpeg: {}", e);
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if start_time.elapsed() >= duration {
                    debug!("Duration exceeded, breaking loop");
                    break;
                }
            }
        }
    }

    debug!("Dropping stdin");
    drop(stdin);
    debug!("Waiting for FFmpeg process to exit");
    let output = ffmpeg.wait_with_output().await?;
    let status = output.status;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    debug!("FFmpeg process exited with status: {}", status);
    debug!("FFmpeg stdout: {}", stdout);
    debug!("FFmpeg stderr: {}", stderr);

    if !status.success() {
        error!("FFmpeg process failed with status: {}", status);
        error!("FFmpeg stderr: {}", stderr);
        return Err(anyhow!("FFmpeg process failed"));
    }

    Ok(())
}

pub async fn record_and_transcribe(
    audio_device: Arc<AudioDevice>,
    duration: Duration,
    output_path: PathBuf,
    whisper_sender: UnboundedSender<AudioInput>,
    is_running: Arc<AtomicBool>,
) -> Result<PathBuf> {
    let (cpal_audio_device, config) = get_device_and_config(&audio_device).await?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as u16;
    debug!(
        "Audio device config: sample_rate={}, channels={}",
        sample_rate, channels
    );

    // TODO: consider a lock-free ring buffer like crossbeam_queue::ArrayQueue (ask AI why)
    let (tx, rx) = mpsc::channel(1000); // For audio data
    let is_running_clone = Arc::clone(&is_running);
    let is_running_clone_2 = is_running.clone();
    let is_running_clone_3 = is_running.clone();
    let is_running_clone_4 = is_running.clone();

    let output_path_clone = Arc::new(output_path);
    let output_path_clone_2 = Arc::clone(&output_path_clone);

    // Define the error callback function
    let error_callback = move |err: StreamError| {
        error!("An error occurred on the audio stream: {}", err);
        if err.to_string().contains("device is no longer valid") {
            warn!("Audio device disconnected. Stopping recording.");
            is_running_clone_2.store(false, Ordering::Relaxed);
        }
    };
    // Spawn a thread to handle the non-Send stream
    thread::spawn(move || {
        let stream = match config.sample_format() {
            cpal::SampleFormat::I8 => cpal_audio_device.build_input_stream(
                &config.into(),
                move |data: &[i8], _: &_| {
                    if is_running_clone_3.load(Ordering::Relaxed) {
                        let _ = tx.blocking_send(bytemuck::cast_slice(data).to_vec());
                    }
                },
                error_callback,
                None,
            ),
            cpal::SampleFormat::I16 => cpal_audio_device.build_input_stream(
                &config.into(),
                move |data: &[i16], _: &_| {
                    if is_running_clone_3.load(Ordering::Relaxed) {
                        let _ = tx.blocking_send(bytemuck::cast_slice(data).to_vec());
                    }
                },
                error_callback,
                None,
            ),
            cpal::SampleFormat::I32 => cpal_audio_device.build_input_stream(
                &config.into(),
                move |data: &[i32], _: &_| {
                    if is_running_clone_3.load(Ordering::Relaxed) {
                        let _ = tx.blocking_send(bytemuck::cast_slice(data).to_vec());
                    }
                },
                error_callback,
                None,
            ),
            cpal::SampleFormat::F32 => cpal_audio_device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &_| {
                    if is_running_clone_3.load(Ordering::Relaxed) {
                        let _ = tx.blocking_send(bytemuck::cast_slice(data).to_vec());
                    }
                },
                error_callback,
                None,
            ),
            _ => {
                error!("Unsupported sample format: {:?}", config.sample_format());
                return;
            }
        };

        match stream {
            Ok(s) => {
                if let Err(e) = s.play() {
                    error!("Failed to play stream: {}", e);
                }
                // Keep the stream alive until the recording is done
                while is_running_clone.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            Err(e) => error!("Failed to build input stream: {}", e),
        }
    });

    info!(
        "Recording {} for {} seconds",
        audio_device.to_string(),
        duration.as_secs()
    );

    // Run FFmpeg in a separate task
    let _ = run_ffmpeg(
        rx,
        sample_rate,
        channels,
        &output_path_clone,
        is_running_clone_4,
        duration,
    )
    .await;

    info!(
        "Recording stopped, wrote to {}. Now triggering transcription",
        output_path_clone_2.to_str().unwrap()
    );

    // Signal the recording thread to stop
    is_running.store(false, Ordering::Relaxed); // TODO: could also just kill the trhead..

    debug!("Sending audio to audio model");
    if let Err(e) = whisper_sender.send(AudioInput {
        path: output_path_clone_2.to_str().unwrap().to_string(),
        device: audio_device.to_string(),
    }) {
        error!("Failed to send audio to audio model: {}", e);
    }
    debug!("Sent audio to audio model");

    Ok(output_path_clone_2.to_path_buf())
}

pub async fn list_audio_devices() -> Result<Vec<AudioDevice>> {
    let host = cpal::default_host();
    let mut devices = Vec::new();

    for device in host.input_devices()? {
        if let Ok(name) = device.name() {
            devices.push(AudioDevice::new(name, DeviceType::Input));
        }
    }

    #[cfg(target_os = "macos")]
    {
        // !HACK macos is suppoed to use special macos feature "display capture"
        // ! see https://github.com/RustAudio/cpal/pull/894
        match get_macos_version().await {
            Some(version) if version < 15.0 => {
                if let Ok(host) = cpal::host_from_id(cpal::HostId::ScreenCaptureKit) {
                    for device in host.input_devices()? {
                        if let Ok(name) = device.name() {
                            devices.push(AudioDevice::new(name, DeviceType::Output));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        for device in host.output_devices()? {
            if let Ok(name) = device.name() {
                devices.push(AudioDevice::new(name, DeviceType::Output));
            }
        }
    }

    Ok(devices)
}

pub fn default_input_device() -> Result<AudioDevice> {
    let host = cpal::default_host();
    let device = host.default_input_device().unwrap();
    Ok(AudioDevice::new(device.name()?, DeviceType::Input))
}

pub async fn default_output_device() -> Result<AudioDevice> {
    #[cfg(target_os = "macos")]
    {
        // ! see https://github.com/RustAudio/cpal/pull/894
        match get_macos_version().await {
            Some(version) if version < 15.0 => {
                if let Ok(host) = cpal::host_from_id(cpal::HostId::ScreenCaptureKit) {
                    if let Some(device) = host.default_input_device() {
                        if let Ok(name) = device.name() {
                            info!("Using display capture device: {}", name);
                            return Ok(AudioDevice::new(name, DeviceType::Output));
                        }
                    }
                }
            }
            _ => {}
        }
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("No default output device found"))?;
        return Ok(AudioDevice::new(device.name()?, DeviceType::Output));
    }

    #[cfg(not(target_os = "macos"))]
    {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("No default output device found"))?;
        return Ok(AudioDevice::new(device.name()?, DeviceType::Output));
    }
}
