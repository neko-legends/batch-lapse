use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
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

#[derive(Debug, Deserialize, Clone)]
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
    output_dir: String,
    ffmpeg_dir: String,
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

fn effective_source_duration(source_duration: f64, options: &SpeedOptions) -> f64 {
    if options.use_clip_length {
        source_duration.min(options.clip_seconds)
    } else {
        source_duration
    }
}

fn output_extension(options: &SpeedOptions) -> &'static str {
    match options.output_format.as_str() {
        "webm-vp9" => "webm",
        "github-gif" => "gif",
        _ => "mp4",
    }
}

fn unique_output_path(input: &Path, options: &SpeedOptions, speed: f64) -> Result<PathBuf, String> {
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
    let suffix = if options.use_clip_length {
        format!("{timing}_clip{}s", target_label(options.clip_seconds))
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
    clip_duration: Option<f64>,
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
    if let Some(clip_duration) = clip_duration {
        command.arg("-t").arg(format!("{clip_duration:.6}"));
    }
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
    clip_duration: Option<f64>,
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
    clip_duration: Option<f64>,
    speed: f64,
    output_duration: f64,
    output_resolution: u32,
    fps: u32,
) -> Result<(), String> {
    let mut command = Command::new(ffmpeg);
    configure_worker_command(&mut command);
    command.args(["-hide_banner", "-nostdin", "-y"]);
    if let Some(clip_duration) = clip_duration {
        command.arg("-t").arg(format!("{clip_duration:.6}"));
    }
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
    clip_duration: Option<f64>,
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
    clip_duration: Option<f64>,
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

#[tauri::command]
fn start_speed_job(
    app: AppHandle,
    active_jobs: State<'_, ActiveJobState>,
    paths: Vec<String>,
    options: SpeedOptions,
) -> Result<String, String> {
    if paths.is_empty() {
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

    let active_jobs_for_thread = active_jobs.inner().clone();
    let app_for_thread = app.clone();
    let job_id_for_thread = job_id.clone();

    thread::spawn(move || {
        let total = paths.len();
        let mut saved = 0usize;
        let mut failed = 0usize;

        for (index, path) in paths.into_iter().enumerate() {
            if active_jobs_for_thread.is_canceled(&job_id_for_thread) {
                break;
            }

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
                let source_duration = effective_source_duration(duration, &options);
                if source_duration <= 0.0 {
                    return Err("Clip duration missing.".to_string());
                }
                let speed = if options.use_target_length {
                    if options.target_seconds >= source_duration {
                        return Err("Target length >= source.".to_string());
                    }
                    source_duration / options.target_seconds
                } else {
                    options.multiplier
                };
                let clip_duration = options.use_clip_length.then_some(source_duration);
                let has_audio = ffprobe_has_audio(&ffprobe, &input);
                let output = unique_output_path(&input, &options, speed)?;
                run_ffmpeg_export(
                    &app_for_thread,
                    &active_jobs_for_thread,
                    &job_id_for_thread,
                    &ffmpeg,
                    &input,
                    &output,
                    clip_duration,
                    speed,
                    source_duration,
                    options.strip_audio,
                    has_audio,
                    &options.output_format,
                    options.output_resolution,
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
fn cancel_active_job(active_jobs: State<'_, ActiveJobState>) -> Result<(), String> {
    let pid = active_jobs.request_cancel()?;
    if let Some(pid) = pid {
        kill_process_tree(pid)?;
    }
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .manage(ActiveJobState::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            cancel_active_job,
            check_runtime,
            open_containing_folder,
            probe_videos,
            resolve_inputs,
            start_speed_job
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}
