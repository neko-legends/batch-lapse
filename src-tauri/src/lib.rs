use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "mkv", "avi", "webm", "wmv", "flv", "mpeg", "mpg", "ts", "mts", "m2ts",
];
const GITHUB_GIF_MAX_SECONDS: f64 = 30.0;
const OUTPUT_RESOLUTIONS: &[u32] = &[480, 720, 1080, 1440];
const MIN_TARGET_SIZE_MB: f64 = 0.1;
const MAX_TARGET_SIZE_MB: f64 = 9.5;
const MIN_VIDEO_BITRATE_KBPS: u32 = 64;
const MIN_GIF_FPS: u32 = 5;
const MAX_GIF_FPS: u32 = 30;
const MIN_GIF_RESOLUTION: u32 = 240;
const GIF_TARGET_ATTEMPTS: usize = 8;
const DIMENSION_REDUCTION_FACTORS: [f64; 5] = [1.0, 1.5, 2.0, 2.5, 3.0];
const DEFAULT_AGENT_API_PORT: u16 = 17336;
const AGENT_APP_ID: &str = "batchlapse";
const AGENT_APP_NAME: &str = "BatchLapse";
const AGENT_API_BIND_ADDRESS: &str = "127.0.0.1";
const AGENT_API_REGISTRY_FILE: &str = "agent-api-registry.json";
const DEFAULT_WINDOW_WIDTH: u32 = 1920;
const DEFAULT_WINDOW_HEIGHT: u32 = 1080;
const MIN_WINDOW_WIDTH: u32 = 640;
const MIN_WINDOW_HEIGHT: u32 = 360;
const LANDSCAPE_WINDOW_WIDTH_RATIO: f64 = 0.5;
const PORTRAIT_WINDOW_WIDTH_RATIO: f64 = 0.9;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    ffmpeg_found: bool,
    ffprobe_found: bool,
    ffmpeg_path: Option<String>,
    ffprobe_path: Option<String>,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoInfo {
    path: String,
    duration_seconds: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    has_audio: bool,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewFrame {
    seconds: f64,
    data_url: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct JobInput {
    path: String,
    start_seconds: Option<f64>,
    end_seconds: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SpeedOptions {
    multiplier: f64,
    use_target_length: bool,
    target_seconds: f64,
    use_clip_length: bool,
    clip_seconds: f64,
    use_target_size: bool,
    target_size_mb: f64,
    gif_fps: u32,
    strip_audio: bool,
    replace_existing: bool,
    output_format: String,
    output_resolution: u32,
    dimension_reduction: f64,
    output_dir: String,
    ffmpeg_dir: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSpeedRequest {
    inputs: Option<Vec<JobInput>>,
    paths: Option<Vec<String>>,
    options: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentServerStatus {
    enabled: bool,
    port: u16,
    url: String,
    openapi_url: String,
    busy: bool,
    active_job_id: Option<String>,
    message: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AgentApiRegistryEntry {
    app_id: String,
    app_name: String,
    default_port: u16,
    bind_address: String,
    port: u16,
    enabled: bool,
    url: String,
    openapi_url: String,
    busy: bool,
    active_job_id: Option<String>,
    last_seen: Option<String>,
    note: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentApiRegistry {
    updated_at: String,
    apps: Vec<AgentApiRegistryEntry>,
}

#[derive(Clone, Default)]
struct ActiveJobState {
    inner: Arc<Mutex<Option<ActiveJob>>>,
}

struct ActiveJob {
    id: String,
    pid: Option<u32>,
    cancel_requested: bool,
}

impl ActiveJobState {
    fn is_busy(&self) -> Result<bool, String> {
        self.inner
            .lock()
            .map(|job| job.is_some())
            .map_err(|_| "Unable to lock active job state.".to_string())
    }

    fn start(&self, id: String) -> Result<(), String> {
        let mut active = self
            .inner
            .lock()
            .map_err(|_| "Unable to lock active job state.".to_string())?;
        if active.is_some() {
            return Err("Another export is already running.".to_string());
        }
        *active = Some(ActiveJob {
            id,
            pid: None,
            cancel_requested: false,
        });
        Ok(())
    }

    fn set_pid(&self, id: &str, pid: u32) -> Result<(), String> {
        let mut active = self
            .inner
            .lock()
            .map_err(|_| "Unable to lock active job state.".to_string())?;
        if let Some(job) = active.as_mut().filter(|job| job.id == id) {
            job.pid = Some(pid);
        }
        Ok(())
    }

    fn clear_pid(&self, id: &str) -> Result<(), String> {
        let mut active = self
            .inner
            .lock()
            .map_err(|_| "Unable to lock active job state.".to_string())?;
        if let Some(job) = active.as_mut().filter(|job| job.id == id) {
            job.pid = None;
        }
        Ok(())
    }

    fn request_cancel(&self) -> Result<Option<u32>, String> {
        let mut active = self
            .inner
            .lock()
            .map_err(|_| "Unable to lock active job state.".to_string())?;
        let Some(job) = active.as_mut() else {
            return Err("No active export to cancel.".to_string());
        };
        job.cancel_requested = true;
        Ok(job.pid)
    }

    fn is_canceled(&self, id: &str) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|job| {
                job.as_ref()
                    .filter(|job| job.id == id)
                    .map(|job| job.cancel_requested)
            })
            .unwrap_or(true)
    }

    fn finish(&self, id: &str) -> Result<bool, String> {
        let mut active = self
            .inner
            .lock()
            .map_err(|_| "Unable to lock active job state.".to_string())?;
        let Some(job) = active.as_ref() else {
            return Ok(false);
        };
        if job.id != id {
            return Ok(false);
        }
        let canceled = job.cancel_requested;
        *active = None;
        Ok(canceled)
    }

    fn active_job_id(&self) -> Result<Option<String>, String> {
        self.inner
            .lock()
            .map(|job| job.as_ref().map(|job| job.id.clone()))
            .map_err(|_| "Unable to lock active job state.".to_string())
    }
}

#[derive(Clone, Default)]
struct AgentServerState {
    inner: Arc<Mutex<AgentServerControl>>,
}

#[derive(Default)]
struct AgentServerControl {
    enabled: bool,
    port: u16,
    stop: Option<Arc<AtomicBool>>,
}

impl AgentServerControl {
    fn port(&self) -> u16 {
        if self.port == 0 {
            read_registered_agent_api_port().unwrap_or(DEFAULT_AGENT_API_PORT)
        } else {
            self.port
        }
    }
}

fn default_speed_options() -> SpeedOptions {
    SpeedOptions {
        multiplier: 2.0,
        use_target_length: false,
        target_seconds: 10.0,
        use_clip_length: false,
        clip_seconds: 60.0,
        use_target_size: false,
        target_size_mb: MAX_TARGET_SIZE_MB,
        gif_fps: 15,
        strip_audio: true,
        replace_existing: false,
        output_format: "mp4-h264".to_string(),
        output_resolution: 720,
        dimension_reduction: 1.0,
        output_dir: String::new(),
        ffmpeg_dir: String::new(),
    }
}

fn hide_command_window(command: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

fn configure_worker_command(command: &mut Command) {
    hide_command_window(command);
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

fn kill_process_tree(pid: u32) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/T", "/F"]);
        command
    };

    #[cfg(not(target_os = "windows"))]
    let mut command = {
        let mut command = Command::new("kill");
        command.args(["-TERM", &format!("-{pid}")]);
        command
    };

    hide_command_window(&mut command);
    match command.output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "Unable to cancel export: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Err(format!("Unable to cancel export: {error}")),
    }
}

fn executable_name(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn default_ffmpeg_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        Some(PathBuf::from(r"D:\Tools\ffmpeg\bin"))
    } else {
        None
    }
}

fn local_binary_candidates(app: &AppHandle, name: &str, ffmpeg_dir: &str) -> Vec<PathBuf> {
    let exe_name = executable_name(name);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let resource_dir = app.path().resource_dir().ok();
    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let selected_dir = ffmpeg_dir.trim();

    [
        (!selected_dir.is_empty()).then(|| PathBuf::from(selected_dir).join(&exe_name)),
        default_ffmpeg_dir().map(|dir| dir.join(&exe_name)),
        Some(current_dir.join("bin").join(&exe_name)),
        Some(current_dir.join(&exe_name)),
        exe_dir.as_ref().map(|dir| dir.join("bin").join(&exe_name)),
        exe_dir.as_ref().map(|dir| dir.join(&exe_name)),
        resource_dir
            .as_ref()
            .map(|dir| dir.join("bin").join(&exe_name)),
        resource_dir.as_ref().map(|dir| dir.join(&exe_name)),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn command_works(program: &Path) -> bool {
    let mut command = Command::new(program);
    command.arg("-version");
    hide_command_window(&mut command);
    matches!(command.output(), Ok(output) if output.status.success())
}

fn path_command_works(name: &str) -> bool {
    let mut command = Command::new(executable_name(name));
    command.arg("-version");
    hide_command_window(&mut command);
    matches!(command.output(), Ok(output) if output.status.success())
}

fn find_binary(app: &AppHandle, name: &str, ffmpeg_dir: &str) -> Option<PathBuf> {
    local_binary_candidates(app, name, ffmpeg_dir)
        .into_iter()
        .find(|path| path.is_file() && command_works(path))
        .or_else(|| {
            if path_command_works(name) {
                Some(PathBuf::from(executable_name(name)))
            } else {
                None
            }
        })
}

fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
}

fn collect_videos(path: &Path, recursive: bool, output: &mut Vec<String>) {
    if path.is_file() {
        if is_video_file(path) {
            output.push(path.display().to_string());
        }
        return;
    }

    if !path.is_dir() {
        return;
    }

    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_file() && is_video_file(&entry_path) {
            output.push(entry_path.display().to_string());
        } else if recursive && entry_path.is_dir() {
            collect_videos(&entry_path, recursive, output);
        }
    }
}

fn ffprobe_duration(ffprobe: &Path, path: &Path) -> Result<f64, String> {
    let mut command = Command::new(ffprobe);
    command.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ]);
    command.arg(path);
    hide_command_window(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Unable to run ffprobe: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| "ffprobe did not return a readable duration.".to_string())
}

fn ffprobe_has_audio(ffprobe: &Path, path: &Path) -> bool {
    let mut command = Command::new(ffprobe);
    command.args([
        "-v",
        "error",
        "-select_streams",
        "a",
        "-show_entries",
        "stream=index",
        "-of",
        "csv=p=0",
    ]);
    command.arg(path);
    hide_command_window(&mut command);
    matches!(command.output(), Ok(output) if output.status.success() && !output.stdout.is_empty())
}

fn ffprobe_resolution(ffprobe: &Path, path: &Path) -> Option<(u32, u32)> {
    let mut command = Command::new(ffprobe);
    command.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height",
        "-of",
        "csv=s=x:p=0",
    ]);
    command.arg(path);
    hide_command_window(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().split('x');
    let width = parts.next()?.parse::<u32>().ok()?;
    let height = parts.next()?.parse::<u32>().ok()?;
    Some((width, height))
}

fn probe_video_with(ffprobe: &Path, path: &str) -> VideoInfo {
    let video_path = PathBuf::from(path);
    match ffprobe_duration(ffprobe, &video_path) {
        Ok(duration) => {
            let resolution = ffprobe_resolution(ffprobe, &video_path);
            VideoInfo {
                path: path.to_string(),
                duration_seconds: Some(duration),
                width: resolution.map(|(width, _)| width),
                height: resolution.map(|(_, height)| height),
                has_audio: ffprobe_has_audio(ffprobe, &video_path),
                error: None,
            }
        }
        Err(error) => VideoInfo {
            path: path.to_string(),
            duration_seconds: None,
            width: None,
            height: None,
            has_audio: false,
            error: Some(error),
        },
    }
}

fn preview_timestamps(duration: f64, count: usize) -> Vec<f64> {
    if duration <= 0.0 || count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![(duration * 0.5).max(0.0)];
    }
    (0..count)
        .map(|index| {
            let ratio = index as f64 / (count - 1) as f64;
            (duration * ratio).min((duration - 0.05).max(0.0))
        })
        .collect()
}

fn generate_preview_frame(
    ffmpeg: &Path,
    input: &Path,
    seconds: f64,
) -> Result<PreviewFrame, String> {
    let mut command = Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .arg("-ss")
        .arg(format!("{seconds:.3}"))
        .arg("-i")
        .arg(input)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale=w='if(gte(iw\\,ih)\\,160\\,-2)':h='if(gte(iw\\,ih)\\,-2\\,90)':flags=lanczos",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ]);
    hide_command_window(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Unable to generate preview frame: {error}"))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(PreviewFrame {
        seconds,
        data_url: format!(
            "data:image/jpeg;base64,{}",
            general_purpose::STANDARD.encode(output.stdout)
        ),
    })
}

fn atempo_chain(mut speed: f64) -> String {
    if speed <= 0.0 {
        return "atempo=1.0".to_string();
    }
    let mut filters = Vec::new();
    while speed > 2.0 {
        filters.push("atempo=2.0".to_string());
        speed /= 2.0;
    }
    while speed < 0.5 {
        filters.push("atempo=0.5".to_string());
        speed /= 0.5;
    }
    filters.push(format!("atempo={speed:.6}"));
    filters.join(",")
}

fn speed_label(speed: f64) -> String {
    let mut label = format!("{speed:.3}");
    while label.contains('.') && label.ends_with('0') {
        label.pop();
    }
    if label.ends_with('.') {
        label.pop();
    }
    label.replace('.', "_")
}

fn target_label(seconds: f64) -> String {
    let mut label = format!("{seconds:.2}");
    while label.contains('.') && label.ends_with('0') {
        label.pop();
    }
    if label.ends_with('.') {
        label.pop();
    }
    label.replace('.', "_")
}

fn normalized_clip_range(
    input: &JobInput,
    source_duration: f64,
    options: &SpeedOptions,
) -> Result<(f64, f64), String> {
    let start = input
        .start_seconds
        .unwrap_or(0.0)
        .clamp(0.0, source_duration);
    let mut duration = input
        .end_seconds
        .unwrap_or(source_duration)
        .clamp(0.0, source_duration)
        - start;
    if options.use_clip_length {
        duration = duration.min(options.clip_seconds);
    }
    if duration <= 0.0 {
        return Err("Clip range must be longer than 0 seconds.".to_string());
    }
    Ok((start, duration))
}

fn output_extension(options: &SpeedOptions) -> &'static str {
    match options.output_format.as_str() {
        "webm-vp9" => "webm",
        "github-gif" => "gif",
        _ => "mp4",
    }
}

fn unique_output_path(
    input: &Path,
    options: &SpeedOptions,
    speed: f64,
    clip_start: f64,
    clip_duration: f64,
    source_duration: f64,
) -> Result<PathBuf, String> {
    let output_dir = if options.output_dir.trim().is_empty() {
        input
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "Unable to resolve source folder.".to_string())?
    } else {
        PathBuf::from(options.output_dir.trim())
    };
    fs::create_dir_all(&output_dir)
        .map_err(|error| format!("Unable to create output folder: {error}"))?;
    let stem = input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("video");
    let timing = if options.use_target_length {
        format!("target{}s", target_label(options.target_seconds))
    } else {
        format!("{}x", speed_label(speed))
    };
    let clip_end = clip_start + clip_duration;
    let has_custom_range =
        clip_start > 0.001 || (source_duration - clip_end).abs() > 0.001 || options.use_clip_length;
    let suffix = if has_custom_range {
        format!(
            "{timing}_range{}s-{}s",
            target_label(clip_start),
            target_label(clip_end)
        )
    } else {
        timing
    };
    let extension = output_extension(options);
    let base = output_dir.join(format!("{stem}_{suffix}.{extension}"));
    if options.replace_existing {
        return Ok(base);
    }
    if !base.exists() {
        return Ok(base);
    }
    for index in 2..10_000 {
        let candidate = output_dir.join(format!("{stem}_{suffix}_{index}.{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("Unable to find an available output filename.".to_string())
}

fn parse_progress_line(line: &str, output_duration: f64) -> Option<f64> {
    let value = line.strip_prefix("out_time_ms=")?;
    let micros = value.trim().parse::<f64>().ok()?;
    if output_duration <= 0.0 {
        return None;
    }
    Some((micros / 1_000_000.0 / output_duration * 100.0).clamp(0.0, 100.0))
}

fn target_size_bytes(options: &SpeedOptions) -> Option<u64> {
    if options.use_target_size {
        Some((options.target_size_mb * 1_000_000.0).round() as u64)
    } else {
        None
    }
}

fn file_size(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("Unable to read output file size: {error}"))
}

fn format_mb(bytes: u64) -> String {
    format!("{:.2} MB", bytes as f64 / 1_000_000.0)
}

fn emit_worker_log(app: &AppHandle, input: &Path, message: impl Into<String>) {
    let _ = app.emit(
        "video-worker-event",
        serde_json::json!({
            "type": "log",
            "path": input.display().to_string(),
            "message": message.into()
        }),
    );
}

fn resolution_filter(output_resolution: u32) -> String {
    format!(
        "scale=w='if(gte(iw\\,ih)\\,-2\\,min({output_resolution}\\,iw))':h='if(gte(iw\\,ih)\\,min({output_resolution}\\,ih)\\,-2)':flags=lanczos"
    )
}

fn reduced_output_resolution(output_resolution: u32, reduction: f64) -> u32 {
    let reduced = ((output_resolution as f64 / reduction).floor() as u32).max(2);
    reduced - (reduced % 2)
}

fn github_gif_filter(speed: f64, output_resolution: u32, fps: u32) -> String {
    format!(
        "[0:v:0]setpts=PTS/{speed:.8},fps={fps},{},split[p0][p1];[p0]palettegen=stats_mode=diff[p];[p1][p]paletteuse=dither=bayer:bayer_scale=5",
        resolution_filter(output_resolution)
    )
}

fn run_ffmpeg_command(
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    job_id: &str,
    mut command: Command,
    input: &Path,
    output: &Path,
    output_duration: f64,
) -> Result<(), String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("Unable to start ffmpeg: {error}"))?;
    let pid = child.id();
    active_jobs.set_pid(job_id, pid)?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let app_for_stderr = app.clone();
    let path_for_stderr = input.display().to_string();
    let stderr_reader = thread::spawn(move || {
        let mut message = String::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_string(&mut message);
        }
        if !message.trim().is_empty() {
            let _ = app_for_stderr.emit(
                "video-worker-event",
                serde_json::json!({
                    "type": "log",
                    "path": path_for_stderr,
                    "message": message.trim()
                }),
            );
        }
    });

    if let Some(stdout) = stdout {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(percent) = parse_progress_line(&line, output_duration) {
                let _ = app.emit(
                    "video-worker-event",
                    serde_json::json!({
                        "type": "video_progress",
                        "path": input.display().to_string(),
                        "percent": percent
                    }),
                );
            }
        }
    }

    let status = child
        .wait()
        .map_err(|error| format!("Unable to wait for ffmpeg: {error}"))?;
    active_jobs.clear_pid(job_id)?;
    let _ = stderr_reader.join();

    if active_jobs.is_canceled(job_id) {
        let _ = fs::remove_file(output);
        return Err("Export canceled.".to_string());
    }
    if !status.success() {
        let _ = fs::remove_file(output);
        return Err("ffmpeg exited with an error.".to_string());
    }
    Ok(())
}

fn audio_bitrate_kbps(strip_audio: bool, has_audio: bool, webm_vp9: bool) -> u32 {
    if strip_audio || !has_audio {
        0
    } else if webm_vp9 {
        128
    } else {
        192
    }
}

fn target_video_bitrate_kbps(
    target_bytes: u64,
    output_duration: f64,
    audio_kbps: u32,
) -> Result<u32, String> {
    let total_kbps = target_bytes as f64 * 8.0 / output_duration / 1000.0;
    let video_kbps = (total_kbps * 0.94) - audio_kbps as f64;
    if video_kbps < MIN_VIDEO_BITRATE_KBPS as f64 {
        return Err(format!("MB target too small for {:.1}s.", output_duration));
    }
    Ok(video_kbps.floor() as u32)
}

fn run_standard_video_pass(
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    job_id: &str,
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    clip_start: f64,
    clip_duration: f64,
    speed: f64,
    output_duration: f64,
    strip_audio: bool,
    has_audio: bool,
    output_resolution: u32,
    webm_vp9: bool,
    target_video_kbps: Option<u32>,
) -> Result<(), String> {
    let mut command = Command::new(ffmpeg);
    configure_worker_command(&mut command);
    command.args(["-hide_banner", "-nostdin", "-y"]);
    if clip_start > 0.0 {
        command.arg("-ss").arg(format!("{clip_start:.6}"));
    }
    command.arg("-t").arg(format!("{clip_duration:.6}"));
    command
        .arg("-i")
        .arg(input)
        .args(["-map", "0:v:0", "-filter:v"])
        .arg(format!(
            "setpts=PTS/{speed:.8},{}",
            resolution_filter(output_resolution)
        ));

    if webm_vp9 {
        command.args(["-c:v", "libvpx-vp9"]);
        if let Some(kbps) = target_video_kbps {
            command.arg("-b:v").arg(format!("{kbps}k"));
        } else {
            command.args(["-crf", "32", "-b:v", "0"]);
        }
    } else {
        command.args(["-c:v", "libx264", "-preset", "medium"]);
        if let Some(kbps) = target_video_kbps {
            command
                .arg("-b:v")
                .arg(format!("{kbps}k"))
                .arg("-maxrate")
                .arg(format!("{kbps}k"))
                .arg("-bufsize")
                .arg(format!("{}k", kbps.saturating_mul(2)));
        } else {
            command.args(["-crf", "20"]);
        }
    }

    if strip_audio || !has_audio {
        command.arg("-an");
    } else {
        let audio_filter = atempo_chain(speed);
        command.args(["-map", "0:a?", "-filter:a"]);
        command.arg(audio_filter);
        if webm_vp9 {
            command.args(["-c:a", "libopus", "-b:a", "128k"]);
        } else {
            command.args(["-c:a", "aac", "-b:a", "192k"]);
        }
    }

    if !webm_vp9 {
        command.args(["-movflags", "+faststart"]);
    }
    command
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_ffmpeg_command(
        app,
        active_jobs,
        job_id,
        command,
        input,
        output,
        output_duration,
    )
}

fn run_standard_video_export(
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    job_id: &str,
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    clip_start: f64,
    clip_duration: f64,
    speed: f64,
    output_duration: f64,
    strip_audio: bool,
    has_audio: bool,
    output_resolution: u32,
    webm_vp9: bool,
    target_bytes: Option<u64>,
) -> Result<(), String> {
    let Some(target_bytes) = target_bytes else {
        return run_standard_video_pass(
            app,
            active_jobs,
            job_id,
            ffmpeg,
            input,
            output,
            clip_start,
            clip_duration,
            speed,
            output_duration,
            strip_audio,
            has_audio,
            output_resolution,
            webm_vp9,
            None,
        );
    };

    let audio_kbps = audio_bitrate_kbps(strip_audio, has_audio, webm_vp9);
    let mut video_kbps = target_video_bitrate_kbps(target_bytes, output_duration, audio_kbps)?;

    for attempt in 1..=4 {
        if attempt > 1 {
            emit_worker_log(
                app,
                input,
                format!(
                    "Retrying size target at {video_kbps} kbps video bitrate for {}.",
                    format_mb(target_bytes)
                ),
            );
        }
        let _ = fs::remove_file(output);
        run_standard_video_pass(
            app,
            active_jobs,
            job_id,
            ffmpeg,
            input,
            output,
            clip_start,
            clip_duration,
            speed,
            output_duration,
            strip_audio,
            has_audio,
            output_resolution,
            webm_vp9,
            Some(video_kbps),
        )?;

        let size = file_size(output)?;
        if size <= target_bytes {
            return Ok(());
        }

        let _ = fs::remove_file(output);
        let ratio = (target_bytes as f64 / size as f64 * 0.92).clamp(0.2, 0.95);
        let next_kbps = (video_kbps as f64 * ratio).floor() as u32;
        video_kbps = if next_kbps >= video_kbps {
            video_kbps.saturating_sub(32)
        } else {
            next_kbps
        };
        if video_kbps < MIN_VIDEO_BITRATE_KBPS {
            break;
        }
    }

    Err("Shorten clip; size floor hit.".to_string())
}

fn run_gif_pass(
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    job_id: &str,
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    clip_start: f64,
    clip_duration: f64,
    speed: f64,
    output_duration: f64,
    output_resolution: u32,
    fps: u32,
) -> Result<(), String> {
    let mut command = Command::new(ffmpeg);
    configure_worker_command(&mut command);
    command.args(["-hide_banner", "-nostdin", "-y"]);
    if clip_start > 0.0 {
        command.arg("-ss").arg(format!("{clip_start:.6}"));
    }
    command.arg("-t").arg(format!("{clip_duration:.6}"));
    command
        .arg("-i")
        .arg(input)
        .args(["-filter_complex"])
        .arg(github_gif_filter(speed, output_resolution, fps))
        .args(["-loop", "0", "-an", "-progress", "pipe:1", "-nostats"])
        .arg(output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_ffmpeg_command(
        app,
        active_jobs,
        job_id,
        command,
        input,
        output,
        output_duration,
    )
}

fn next_gif_settings(
    target_bytes: u64,
    actual_bytes: u64,
    current_resolution: u32,
    current_fps: u32,
) -> Option<(u32, u32)> {
    let ratio = (target_bytes as f64 / actual_bytes as f64).clamp(0.18, 0.9);
    let factor = ratio.powf(0.45);
    let mut next_fps = (current_fps as f64 * factor).floor() as u32;
    let mut next_resolution = (current_resolution as f64 * factor).floor() as u32;

    next_fps = next_fps.max(MIN_GIF_FPS);
    next_resolution = (next_resolution / 2 * 2).max(MIN_GIF_RESOLUTION);

    if next_fps >= current_fps && current_fps > MIN_GIF_FPS {
        next_fps = current_fps - 1;
    }
    if next_resolution >= current_resolution && current_resolution > MIN_GIF_RESOLUTION {
        next_resolution = current_resolution
            .saturating_sub(80)
            .max(MIN_GIF_RESOLUTION);
        next_resolution = next_resolution / 2 * 2;
    }

    if next_fps == current_fps && next_resolution == current_resolution {
        None
    } else {
        Some((next_resolution, next_fps))
    }
}

fn run_gif_export(
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    job_id: &str,
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    clip_start: f64,
    clip_duration: f64,
    speed: f64,
    output_duration: f64,
    output_resolution: u32,
    initial_fps: u32,
    target_bytes: Option<u64>,
) -> Result<(), String> {
    let Some(target_bytes) = target_bytes else {
        if output_duration > GITHUB_GIF_MAX_SECONDS {
            return Err("GIF >30s; use MB target.".to_string());
        }

        return run_gif_pass(
            app,
            active_jobs,
            job_id,
            ffmpeg,
            input,
            output,
            clip_start,
            clip_duration,
            speed,
            output_duration,
            output_resolution,
            initial_fps,
        );
    };

    let mut gif_resolution = output_resolution;
    let mut fps = initial_fps;

    for attempt in 1..=GIF_TARGET_ATTEMPTS {
        if attempt > 1 {
            emit_worker_log(
                app,
                input,
                format!(
                    "Retrying GIF size target at {fps} fps and {gif_resolution}p for {}.",
                    format_mb(target_bytes)
                ),
            );
        }
        let _ = fs::remove_file(output);
        run_gif_pass(
            app,
            active_jobs,
            job_id,
            ffmpeg,
            input,
            output,
            clip_start,
            clip_duration,
            speed,
            output_duration,
            gif_resolution,
            fps,
        )?;

        let size = file_size(output)?;
        if size <= target_bytes {
            return Ok(());
        }

        let _ = fs::remove_file(output);
        let Some((next_resolution, next_fps)) =
            next_gif_settings(target_bytes, size, gif_resolution, fps)
        else {
            break;
        };
        gif_resolution = next_resolution;
        fps = next_fps;
    }

    Err("Shorten clip; size floor hit.".to_string())
}

fn run_ffmpeg_export(
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    job_id: &str,
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    clip_start: f64,
    clip_duration: f64,
    speed: f64,
    duration: f64,
    strip_audio: bool,
    has_audio: bool,
    output_format: &str,
    output_resolution: u32,
    gif_fps: u32,
    target_bytes: Option<u64>,
) -> Result<(), String> {
    let output_duration = duration / speed;
    let github_gif = output_format.eq_ignore_ascii_case("github-gif");
    let webm_vp9 = output_format.eq_ignore_ascii_case("webm-vp9");

    if github_gif {
        return run_gif_export(
            app,
            active_jobs,
            job_id,
            ffmpeg,
            input,
            output,
            clip_start,
            clip_duration,
            speed,
            output_duration,
            output_resolution,
            gif_fps,
            target_bytes,
        );
    }

    run_standard_video_export(
        app,
        active_jobs,
        job_id,
        ffmpeg,
        input,
        output,
        clip_start,
        clip_duration,
        speed,
        output_duration,
        strip_audio,
        has_audio,
        output_resolution,
        webm_vp9,
        target_bytes,
    )
}

fn missing_binary_message(binary: &str, ffmpeg_dir: &str) -> String {
    let selected = ffmpeg_dir.trim();
    let binary_name = executable_name(binary);
    if selected.is_empty() {
        if cfg!(target_os = "windows") {
            format!(
                "{binary_name} was not found. Choose its folder, use D:\\Tools\\ffmpeg\\bin, add it to PATH, or place it in this app's bin folder."
            )
        } else {
            format!(
                "{binary_name} was not found. Choose its folder, add it to PATH, or place it in this app's bin folder."
            )
        }
    } else {
        format!(
            "{binary_name} was not found in {selected}. Choose the folder containing {} and {}.",
            executable_name("ffmpeg"),
            executable_name("ffprobe")
        )
    }
}

#[tauri::command]
fn check_runtime(app: AppHandle, ffmpeg_dir: String) -> RuntimeStatus {
    let ffmpeg = find_binary(&app, "ffmpeg", &ffmpeg_dir);
    let ffprobe = find_binary(&app, "ffprobe", &ffmpeg_dir);
    let message = match (&ffmpeg, &ffprobe) {
        (Some(_), Some(_)) => "ffmpeg and ffprobe are ready.".to_string(),
        (None, Some(_)) => missing_binary_message("ffmpeg", &ffmpeg_dir),
        (Some(_), None) => missing_binary_message("ffprobe", &ffmpeg_dir),
        (None, None) => format!(
            "{} {}",
            missing_binary_message("ffmpeg", &ffmpeg_dir),
            missing_binary_message("ffprobe", &ffmpeg_dir)
        ),
    };
    RuntimeStatus {
        ffmpeg_found: ffmpeg.is_some(),
        ffprobe_found: ffprobe.is_some(),
        ffmpeg_path: ffmpeg.map(|path| path.display().to_string()),
        ffprobe_path: ffprobe.map(|path| path.display().to_string()),
        message,
    }
}

#[tauri::command]
fn resolve_inputs(paths: Vec<String>, recursive: bool) -> Vec<String> {
    let mut output = Vec::new();
    for path in paths {
        collect_videos(Path::new(&path), recursive, &mut output);
    }
    output.sort();
    output.dedup();
    output
}

#[tauri::command]
fn probe_videos(
    app: AppHandle,
    paths: Vec<String>,
    ffmpeg_dir: String,
) -> Result<Vec<VideoInfo>, String> {
    let ffprobe = find_binary(&app, "ffprobe", &ffmpeg_dir)
        .ok_or_else(|| missing_binary_message("ffprobe", &ffmpeg_dir))?;
    Ok(paths
        .into_iter()
        .map(|path| probe_video_with(&ffprobe, &path))
        .collect())
}

#[tauri::command]
fn generate_preview_frames(
    app: AppHandle,
    path: String,
    ffmpeg_dir: String,
) -> Result<Vec<PreviewFrame>, String> {
    let ffmpeg = find_binary(&app, "ffmpeg", &ffmpeg_dir)
        .ok_or_else(|| missing_binary_message("ffmpeg", &ffmpeg_dir))?;
    let ffprobe = find_binary(&app, "ffprobe", &ffmpeg_dir)
        .ok_or_else(|| missing_binary_message("ffprobe", &ffmpeg_dir))?;
    let input = PathBuf::from(path);
    let duration = ffprobe_duration(&ffprobe, &input)?;
    let frames = preview_timestamps(duration, 7)
        .into_iter()
        .filter_map(|seconds| generate_preview_frame(&ffmpeg, &input, seconds).ok())
        .collect::<Vec<_>>();
    if frames.is_empty() {
        return Err("Could not generate preview frames.".to_string());
    }
    Ok(frames)
}

#[tauri::command]
fn open_containing_folder(app: AppHandle, path: String) -> Result<(), String> {
    let requested_path = PathBuf::from(path);
    let folder = if requested_path.is_dir() {
        requested_path
    } else {
        requested_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "Unable to resolve containing folder.".to_string())?
    };
    if !folder.is_dir() {
        return Err(format!("Folder does not exist: {}", folder.display()));
    }
    app.opener()
        .open_path(folder.display().to_string(), None::<&str>)
        .map_err(|error| format!("Unable to open folder: {error}"))
}

fn start_speed_job_core(
    app: AppHandle,
    active_jobs: &ActiveJobState,
    inputs: Vec<JobInput>,
    options: SpeedOptions,
) -> Result<String, String> {
    if inputs.is_empty() {
        return Err("No input videos were provided.".to_string());
    }
    if active_jobs.is_busy()? {
        return Err("Another export is already running.".to_string());
    }
    if options.use_target_length && options.target_seconds <= 0.0 {
        return Err("Target length must be greater than 0 seconds.".to_string());
    }
    if options.use_clip_length && options.clip_seconds <= 0.0 {
        return Err("Clip length must be greater than 0 seconds.".to_string());
    }
    if !options.use_target_length && !(1.0..=10.0).contains(&options.multiplier) {
        return Err("Multiplier must be between 1x and 10x.".to_string());
    }
    if !["mp4-h264", "webm-vp9", "github-gif"].contains(&options.output_format.as_str()) {
        return Err("Unsupported output format.".to_string());
    }
    if !OUTPUT_RESOLUTIONS.contains(&options.output_resolution) {
        return Err("Output resolution must be 480p, 720p, 1080p, or 1440p.".to_string());
    }
    if !DIMENSION_REDUCTION_FACTORS.contains(&options.dimension_reduction) {
        return Err("Dimension reduction must be 1x, 1.5x, 2x, 2.5x, or 3x.".to_string());
    }
    if options.use_target_size
        && !(MIN_TARGET_SIZE_MB..=MAX_TARGET_SIZE_MB).contains(&options.target_size_mb)
    {
        return Err("Target size must be 0.1-9.5 MB.".to_string());
    }
    if !(MIN_GIF_FPS..=MAX_GIF_FPS).contains(&options.gif_fps) {
        return Err("GIF FPS must be 5-30.".to_string());
    }
    let ffmpeg = find_binary(&app, "ffmpeg", &options.ffmpeg_dir)
        .ok_or_else(|| missing_binary_message("ffmpeg", &options.ffmpeg_dir))?;
    let ffprobe = find_binary(&app, "ffprobe", &options.ffmpeg_dir)
        .ok_or_else(|| missing_binary_message("ffprobe", &options.ffmpeg_dir))?;
    let job_id = Uuid::new_v4().to_string();
    active_jobs.start(job_id.clone())?;

    let active_jobs_for_thread = active_jobs.clone();
    let app_for_thread = app.clone();
    let job_id_for_thread = job_id.clone();

    thread::spawn(move || {
        let total = inputs.len();
        let mut saved = 0usize;
        let mut failed = 0usize;

        for (index, input_item) in inputs.into_iter().enumerate() {
            if active_jobs_for_thread.is_canceled(&job_id_for_thread) {
                break;
            }

            let path = input_item.path.clone();
            let input = PathBuf::from(&path);
            let _ = app_for_thread.emit(
                "video-worker-event",
                serde_json::json!({
                    "type": "video_start",
                    "path": path.clone(),
                    "message": format!("Exporting {} of {}", index + 1, total)
                }),
            );

            let result = (|| {
                let duration = ffprobe_duration(&ffprobe, &input)?;
                if duration <= 0.0 {
                    return Err("Video duration missing.".to_string());
                }
                let (clip_start, clip_duration) =
                    normalized_clip_range(&input_item, duration, &options)?;
                let speed = if options.use_target_length {
                    if options.target_seconds >= clip_duration {
                        return Err("Target length >= source.".to_string());
                    }
                    clip_duration / options.target_seconds
                } else {
                    options.multiplier
                };
                let has_audio = ffprobe_has_audio(&ffprobe, &input);
                let output = unique_output_path(
                    &input,
                    &options,
                    speed,
                    clip_start,
                    clip_duration,
                    duration,
                )?;
                run_ffmpeg_export(
                    &app_for_thread,
                    &active_jobs_for_thread,
                    &job_id_for_thread,
                    &ffmpeg,
                    &input,
                    &output,
                    clip_start,
                    clip_duration,
                    speed,
                    clip_duration,
                    options.strip_audio,
                    has_audio,
                    &options.output_format,
                    reduced_output_resolution(
                        options.output_resolution,
                        options.dimension_reduction,
                    ),
                    options.gif_fps,
                    target_size_bytes(&options),
                )?;
                Ok((output, speed))
            })();

            match result {
                Ok((output, speed)) => {
                    saved += 1;
                    let _ = app_for_thread.emit(
                        "video-worker-event",
                        serde_json::json!({
                            "type": "video_done",
                            "path": path.clone(),
                            "output": output.display().to_string(),
                            "speed": speed,
                            "message": format!("Saved {}", output.display())
                        }),
                    );
                }
                Err(error) => {
                    if active_jobs_for_thread.is_canceled(&job_id_for_thread) {
                        break;
                    }
                    failed += 1;
                    let _ = app_for_thread.emit(
                        "video-worker-event",
                        serde_json::json!({
                            "type": "video_error",
                            "path": path.clone(),
                            "message": error
                        }),
                    );
                }
            }
        }

        let was_canceled = active_jobs_for_thread
            .finish(&job_id_for_thread)
            .unwrap_or(false);
        if was_canceled {
            let _ = app_for_thread.emit(
                "video-worker-event",
                serde_json::json!({ "type": "canceled", "message": "Export canceled." }),
            );
        } else {
            let _ = app_for_thread.emit(
                "video-worker-event",
                serde_json::json!({
                    "type": "done",
                    "message": format!("Finished. Saved {saved}, failed {failed}.")
                }),
            );
        }
    });

    Ok(job_id)
}

#[tauri::command]
fn start_speed_job(
    app: AppHandle,
    active_jobs: State<'_, ActiveJobState>,
    inputs: Vec<JobInput>,
    options: SpeedOptions,
) -> Result<String, String> {
    start_speed_job_core(app, active_jobs.inner(), inputs, options)
}

#[tauri::command]
fn cancel_active_job(active_jobs: State<'_, ActiveJobState>) -> Result<(), String> {
    let pid = active_jobs.request_cancel()?;
    if let Some(pid) = pid {
        kill_process_tree(pid)?;
    }
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn validate_agent_api_port(port: Option<u16>) -> Result<u16, String> {
    let port = port
        .or_else(read_registered_agent_api_port)
        .unwrap_or(DEFAULT_AGENT_API_PORT);
    if port == 0 {
        return Err("Agent API port must be between 1 and 65535.".to_string());
    }
    Ok(port)
}

fn agent_api_url(port: u16) -> String {
    format!("http://{AGENT_API_BIND_ADDRESS}:{port}")
}

fn timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn shared_neko_legends_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
    } else if cfg!(target_os = "macos") {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support"))
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("NekoLegends"))
}

fn agent_api_registry_path() -> Option<PathBuf> {
    Some(shared_neko_legends_dir()?.join(AGENT_API_REGISTRY_FILE))
}

fn read_agent_api_registry() -> AgentApiRegistry {
    let updated_at = timestamp_string();
    let Some(path) = agent_api_registry_path() else {
        return AgentApiRegistry {
            updated_at,
            apps: Vec::new(),
        };
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(AgentApiRegistry {
            updated_at,
            apps: Vec::new(),
        })
}

fn read_registered_agent_api_port() -> Option<u16> {
    read_agent_api_registry()
        .apps
        .into_iter()
        .find(|entry| entry.app_id == AGENT_APP_ID)
        .map(|entry| entry.port)
        .filter(|port| *port > 0)
}

fn publish_agent_api_status(status: &AgentServerStatus) {
    let Some(path) = agent_api_registry_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut registry = read_agent_api_registry();
    let updated_at = timestamp_string();
    let entry = AgentApiRegistryEntry {
        app_id: AGENT_APP_ID.to_string(),
        app_name: AGENT_APP_NAME.to_string(),
        default_port: DEFAULT_AGENT_API_PORT,
        bind_address: AGENT_API_BIND_ADDRESS.to_string(),
        port: status.port,
        enabled: status.enabled,
        url: status.url.clone(),
        openapi_url: status.openapi_url.clone(),
        busy: status.busy,
        active_job_id: status.active_job_id.clone(),
        last_seen: Some(updated_at.clone()),
        note: Some("Local Agent API.".to_string()),
    };
    if let Some(existing) = registry
        .apps
        .iter_mut()
        .find(|entry| entry.app_id == AGENT_APP_ID)
    {
        *existing = entry;
    } else {
        registry.apps.push(entry);
    }
    registry.updated_at = updated_at;
    if let Ok(raw) = serde_json::to_string_pretty(&registry) {
        let _ = fs::write(path, raw);
    }
}

fn agent_status_from(
    agent_state: &AgentServerState,
    active_jobs: &ActiveJobState,
) -> Result<AgentServerStatus, String> {
    let (enabled, port) = {
        let control = agent_state
            .inner
            .lock()
            .map_err(|_| "Unable to lock agent server state.".to_string())?;
        (control.enabled, control.port())
    };
    let active_job_id = active_jobs.active_job_id()?;
    let status = AgentServerStatus {
        enabled,
        port,
        url: agent_api_url(port),
        openapi_url: format!("{}/openapi.json", agent_api_url(port)),
        busy: active_job_id.is_some(),
        active_job_id,
        message: if enabled {
            "Agent API is enabled.".to_string()
        } else {
            "Agent API is off.".to_string()
        },
    };
    publish_agent_api_status(&status);
    Ok(status)
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("Unable to set read timeout: {error}"))?;
    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected_len: Option<usize> = None;

    loop {
        let bytes_read = stream
            .read(&mut buffer)
            .map_err(|error| format!("Unable to read agent request: {error}"))?;
        if bytes_read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..bytes_read]);
        if let Some(header_end) = find_header_end(&data) {
            if expected_len.is_none() {
                let headers = String::from_utf8_lossy(&data[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                expected_len = Some(header_end + 4 + content_length);
            }
            if expected_len.is_some_and(|len| data.len() >= len) {
                break;
            }
        }
        if data.len() > 2 * 1024 * 1024 {
            return Err("Agent request is too large.".to_string());
        }
    }

    let header_end = find_header_end(&data).ok_or_else(|| "Invalid HTTP request.".to_string())?;
    let headers = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "Invalid HTTP request line.".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();
    let path = raw_path.split('?').next().unwrap_or("/").to_string();
    let body_start = header_end + 4;
    let body = if body_start <= data.len() {
        data[body_start..].to_vec()
    } else {
        Vec::new()
    };
    Ok(HttpRequest { method, path, body })
}

fn write_json_response(
    stream: &mut TcpStream,
    status: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let body = serde_json::to_vec(&payload)
        .map_err(|error| format!("Unable to serialize agent response: {error}"))?;
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|error| format!("Unable to write agent response: {error}"))
}

fn write_empty_response(stream: &mut TcpStream, status: &str) -> Result<(), String> {
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(headers.as_bytes())
        .map_err(|error| format!("Unable to write agent response: {error}"))
}

fn agent_openapi(port: u16) -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "BatchLapse Agent API",
            "version": env!("CARGO_PKG_VERSION")
        },
        "servers": [{ "url": agent_api_url(port) }],
        "paths": {
            "/health": { "get": { "summary": "Check API status" } },
            "/status": { "get": { "summary": "Check active export status" } },
            "/runtime": { "get": { "summary": "Check FFmpeg readiness with default settings" } },
            "/export": { "post": { "summary": "Start a batch speed export" } },
            "/generate": { "post": { "summary": "Alias for /export" } },
            "/cancel": { "post": { "summary": "Cancel the active export" } }
        }
    })
}

fn parse_agent_speed_request(body: &[u8]) -> Result<AgentSpeedRequest, String> {
    if body.is_empty() {
        return Ok(AgentSpeedRequest {
            inputs: None,
            paths: None,
            options: None,
        });
    }
    serde_json::from_slice(body).map_err(|error| format!("Invalid JSON request: {error}"))
}

fn agent_options_from_request(options: Option<serde_json::Value>) -> Result<SpeedOptions, String> {
    let mut merged = serde_json::to_value(default_speed_options())
        .map_err(|error| format!("Unable to build default options: {error}"))?;
    let Some(options) = options else {
        return serde_json::from_value(merged)
            .map_err(|error| format!("Unable to read default options: {error}"));
    };
    if options.is_null() {
        return serde_json::from_value(merged)
            .map_err(|error| format!("Unable to read default options: {error}"));
    }
    let overrides = options
        .as_object()
        .ok_or_else(|| "Agent options must be a JSON object.".to_string())?;
    let base = merged
        .as_object_mut()
        .ok_or_else(|| "Unable to merge default options.".to_string())?;
    for (key, value) in overrides {
        base.insert(key.clone(), value.clone());
    }
    serde_json::from_value(merged).map_err(|error| format!("Invalid agent options: {error}"))
}

fn agent_inputs_from_request(
    inputs: Option<Vec<JobInput>>,
    paths: Option<Vec<String>>,
    recursive: bool,
) -> Vec<JobInput> {
    if let Some(inputs) = inputs {
        return inputs;
    }
    resolve_inputs(paths.unwrap_or_default(), recursive)
        .into_iter()
        .map(|path| JobInput {
            path,
            start_seconds: None,
            end_seconds: None,
        })
        .collect()
}

fn handle_agent_route(
    request: HttpRequest,
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    agent_state: &AgentServerState,
) -> Result<serde_json::Value, String> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => Ok(serde_json::json!({
            "ok": true,
            "service": "BatchLapse",
            "version": env!("CARGO_PKG_VERSION"),
            "url": agent_status_from(agent_state, active_jobs)?.url
        })),
        ("GET", "/openapi.json") => Ok(agent_openapi(
            agent_status_from(agent_state, active_jobs)?.port,
        )),
        ("GET", "/status") => serde_json::to_value(agent_status_from(agent_state, active_jobs)?)
            .map_err(|error| error.to_string()),
        ("GET", "/runtime") => serde_json::to_value(check_runtime(app.clone(), String::new()))
            .map_err(|error| error.to_string()),
        ("POST", "/export") | ("POST", "/generate") => {
            let request = parse_agent_speed_request(&request.body)?;
            let AgentSpeedRequest {
                inputs,
                paths,
                options,
            } = request;
            let options = agent_options_from_request(options)?;
            let inputs = agent_inputs_from_request(inputs, paths, true);
            let job_id = start_speed_job_core(app.clone(), active_jobs, inputs, options)?;
            Ok(serde_json::json!({ "ok": true, "jobId": job_id }))
        }
        ("POST", "/cancel") => {
            let pid = active_jobs.request_cancel()?;
            if let Some(pid) = pid {
                kill_process_tree(pid)?;
            }
            Ok(serde_json::json!({ "ok": true }))
        }
        _ => Err(format!(
            "No agent endpoint for {} {}",
            request.method, request.path
        )),
    }
}

fn handle_agent_stream(
    mut stream: TcpStream,
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    agent_state: &AgentServerState,
) {
    let result = read_http_request(&mut stream).and_then(|request| {
        if request.method == "OPTIONS" {
            return write_empty_response(&mut stream, "204 No Content");
        }
        match handle_agent_route(request, app, active_jobs, agent_state) {
            Ok(payload) => write_json_response(&mut stream, "200 OK", payload),
            Err(error) => write_json_response(
                &mut stream,
                "400 Bad Request",
                serde_json::json!({ "ok": false, "error": error }),
            ),
        }
    });
    if let Err(error) = result {
        let _ = write_json_response(
            &mut stream,
            "400 Bad Request",
            serde_json::json!({ "ok": false, "error": error }),
        );
    }
}

fn run_agent_server(
    listener: TcpListener,
    app: AppHandle,
    active_jobs: ActiveJobState,
    agent_state: AgentServerState,
    stop: Arc<AtomicBool>,
) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => handle_agent_stream(stream, &app, &active_jobs, &agent_state),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(80));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(150));
            }
        }
    }
}

#[tauri::command]
fn get_agent_server_status(
    agent_state: State<'_, AgentServerState>,
    active_jobs: State<'_, ActiveJobState>,
) -> Result<AgentServerStatus, String> {
    agent_status_from(agent_state.inner(), active_jobs.inner())
}

fn set_agent_server_enabled_inner(
    app: AppHandle,
    agent_state: &AgentServerState,
    active_jobs: &ActiveJobState,
    enabled: bool,
    port: Option<u16>,
) -> Result<AgentServerStatus, String> {
    let port = validate_agent_api_port(port)?;
    {
        let mut control = agent_state
            .inner
            .lock()
            .map_err(|_| "Unable to lock agent server state.".to_string())?;

        if control.enabled && (!enabled || control.port() != port) {
            if let Some(stop) = control.stop.take() {
                stop.store(true, Ordering::SeqCst);
            }
            control.enabled = false;
        }
        control.port = port;

        if enabled && !control.enabled {
            let listener = TcpListener::bind(("127.0.0.1", port))
                .map_err(|error| format!("Unable to start Agent API: {error}"))?;
            let stop = Arc::new(AtomicBool::new(false));
            thread::spawn({
                let app = app.clone();
                let active_jobs = active_jobs.clone();
                let agent_state = agent_state.clone();
                let stop = stop.clone();
                move || run_agent_server(listener, app, active_jobs, agent_state, stop)
            });
            control.enabled = true;
            control.stop = Some(stop);
        }
    }

    agent_status_from(agent_state, active_jobs)
}

#[tauri::command]
fn set_agent_server_enabled(
    app: AppHandle,
    agent_state: State<'_, AgentServerState>,
    active_jobs: State<'_, ActiveJobState>,
    enabled: bool,
    port: Option<u16>,
) -> Result<AgentServerStatus, String> {
    set_agent_server_enabled_inner(app, agent_state.inner(), active_jobs.inner(), enabled, port)
}

fn env_truthy(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn agent_api_headless_requested() -> bool {
    env::args().any(|arg| arg == "--headless" || arg == "--serve-agent-api")
        || env_truthy("NEKO_AGENT_HEADLESS")
        || env_truthy("BATCHLAPSE_HEADLESS")
}

fn agent_api_cli_port() -> Option<u16> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--agent-api-port" || arg == "--agent-port" {
            return args.next().and_then(|value| value.parse::<u16>().ok());
        }
        if let Some(value) = arg
            .strip_prefix("--agent-api-port=")
            .or_else(|| arg.strip_prefix("--agent-port="))
        {
            return value.parse::<u16>().ok();
        }
    }
    env::var("BATCHLAPSE_AGENT_API_PORT")
        .or_else(|_| env::var("NEKO_AGENT_API_PORT"))
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
}

fn hide_main_window(app: &tauri::App) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

fn clamp_window_dimension(value: u32, min: u32, monitor_max: u32) -> u32 {
    let max = monitor_max.max(1);
    value.max(min.min(max)).min(max)
}

fn initial_window_size(monitor_width: u32, monitor_height: u32) -> tauri::PhysicalSize<u32> {
    let width_ratio = if monitor_width < monitor_height {
        PORTRAIT_WINDOW_WIDTH_RATIO
    } else {
        LANDSCAPE_WINDOW_WIDTH_RATIO
    };
    let aspect = DEFAULT_WINDOW_HEIGHT as f64 / DEFAULT_WINDOW_WIDTH as f64;
    let mut width = (monitor_width as f64 * width_ratio).round();
    let mut height = (width * aspect).round();
    let max_height = (monitor_height as f64 * 0.9).round().max(1.0);

    if height > max_height {
        height = max_height;
        width = (height / aspect).round();
    }

    tauri::PhysicalSize::new(width.max(1.0) as u32, height.max(1.0) as u32)
}

fn apply_initial_window_size(app: &tauri::App) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let monitor_size = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())
        .map(|monitor| *monitor.size());
    let monitor_width = monitor_size
        .as_ref()
        .map(|size| size.width)
        .unwrap_or(DEFAULT_WINDOW_WIDTH);
    let monitor_height = monitor_size
        .as_ref()
        .map(|size| size.height)
        .unwrap_or(DEFAULT_WINDOW_HEIGHT);
    let size = initial_window_size(monitor_width, monitor_height);
    let size = tauri::PhysicalSize::new(
        clamp_window_dimension(size.width, MIN_WINDOW_WIDTH, monitor_width),
        clamp_window_dimension(size.height, MIN_WINDOW_HEIGHT, monitor_height),
    );

    if let Err(error) = window.set_size(tauri::Size::Physical(size)) {
        eprintln!("Failed to initialize window size: {error}");
    }
    let _ = window.center();
}

pub fn run() {
    tauri::Builder::default()
        .manage(ActiveJobState::default())
        .manage(AgentServerState::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let headless = agent_api_headless_requested();
            if !headless {
                apply_initial_window_size(app);
            }
            if headless {
                hide_main_window(app);
                let handle = app.handle().clone();
                let agent_state = app.state::<AgentServerState>().inner().clone();
                let active_jobs = app.state::<ActiveJobState>().inner().clone();
                if let Err(error) = set_agent_server_enabled_inner(
                    handle,
                    &agent_state,
                    &active_jobs,
                    true,
                    agent_api_cli_port(),
                ) {
                    eprintln!("Unable to start Agent API: {error}");
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cancel_active_job,
            check_runtime,
            generate_preview_frames,
            get_agent_server_status,
            open_containing_folder,
            probe_videos,
            resolve_inputs,
            set_agent_server_enabled,
            start_speed_job
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}
