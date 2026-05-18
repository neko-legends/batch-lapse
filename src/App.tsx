import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow, PhysicalSize } from "@tauri-apps/api/window";
import { APP_VERSION, BUILD_HASH } from "./build-info";
import {
  CheckCircle2,
  Clock3,
  Download,
  FolderOpen,
  Gauge,
  Loader2,
  Play,
  RotateCcw,
  Scissors,
  Square,
  TriangleAlert,
  Video,
  XCircle
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

type QueueStatus = "pending" | "running" | "done" | "error" | "canceled";
type OutputFormat = "mp4-h264" | "webm-vp9" | "github-gif";

type RuntimeStatus = {
  ffmpegFound: boolean;
  ffprobeFound: boolean;
  ffmpegPath: string | null;
  ffprobePath: string | null;
  message: string;
};

type VideoInfo = {
  path: string;
  durationSeconds: number | null;
  hasAudio: boolean;
  error: string | null;
};

type SpeedOptions = {
  multiplier: number;
  useTargetLength: boolean;
  targetSeconds: number;
  stripAudio: boolean;
  replaceExisting: boolean;
  outputFormat: OutputFormat;
  githubGifMaxMb: number;
  outputDir: string;
  ffmpegDir: string;
  recursive: boolean;
};

type QueueItem = {
  path: string;
  status: QueueStatus;
  durationSeconds?: number | null;
  hasAudio?: boolean;
  progress?: number;
  output?: string;
  speed?: number;
  message?: string;
};

type WorkerEvent = {
  type:
    | "video_start"
    | "video_progress"
    | "video_done"
    | "video_error"
    | "done"
    | "canceled"
    | "log";
  path?: string;
  output?: string;
  speed?: number;
  percent?: number;
  message?: string;
};

const SETTINGS_STORAGE_KEY = "batchlapse.settings.v1";
const VIDEO_EXTENSIONS = ["mp4", "mov", "m4v", "mkv", "avi", "webm", "wmv", "flv", "mpeg", "mpg", "ts", "mts", "m2ts"];
const OUTPUT_FORMAT_OPTIONS: OutputFormat[] = ["mp4-h264", "webm-vp9", "github-gif"];

const defaultOptions: SpeedOptions = {
  multiplier: 2,
  useTargetLength: false,
  targetSeconds: 10,
  stripAudio: true,
  replaceExisting: false,
  outputFormat: "mp4-h264",
  githubGifMaxMb: 9,
  outputDir: "",
  ffmpegDir: "",
  recursive: true
};

const isTauriRuntime = () => "__TAURI_INTERNALS__" in window || "__TAURI__" in window;
const fileName = (path: string) => path.split(/[\\/]/).pop() ?? path;

function coerceNumber(value: unknown, fallback: number, min: number, max: number) {
  return typeof value === "number" && Number.isFinite(value)
    ? Math.max(min, Math.min(max, value))
    : fallback;
}

function coerceBoolean(value: unknown, fallback: boolean) {
  return typeof value === "boolean" ? value : fallback;
}

function coerceChoice<T extends string>(value: unknown, choices: T[], fallback: T) {
  return typeof value === "string" && choices.includes(value as T) ? (value as T) : fallback;
}

function loadSavedOptions(): SpeedOptions {
  if (typeof window === "undefined") return defaultOptions;
  try {
    const saved = window.localStorage.getItem(SETTINGS_STORAGE_KEY);
    if (!saved) return defaultOptions;
    const parsed = JSON.parse(saved) as Partial<SpeedOptions>;
    return {
      multiplier: coerceNumber(parsed.multiplier, defaultOptions.multiplier, 1, 10),
      useTargetLength: coerceBoolean(parsed.useTargetLength, defaultOptions.useTargetLength),
      targetSeconds: coerceNumber(parsed.targetSeconds, defaultOptions.targetSeconds, 0.1, 86400),
      stripAudio: coerceBoolean(parsed.stripAudio, defaultOptions.stripAudio),
      replaceExisting: coerceBoolean(parsed.replaceExisting, defaultOptions.replaceExisting),
      outputFormat: coerceChoice(parsed.outputFormat, OUTPUT_FORMAT_OPTIONS, defaultOptions.outputFormat),
      githubGifMaxMb: coerceNumber(parsed.githubGifMaxMb, defaultOptions.githubGifMaxMb, 1, 9),
      outputDir: typeof parsed.outputDir === "string" ? parsed.outputDir : "",
      ffmpegDir: typeof parsed.ffmpegDir === "string" ? parsed.ffmpegDir : defaultOptions.ffmpegDir,
      recursive: coerceBoolean(parsed.recursive, defaultOptions.recursive)
    };
  } catch {
    return defaultOptions;
  }
}

function formatDuration(seconds?: number | null) {
  if (!seconds || !Number.isFinite(seconds)) return "--";
  const rounded = Math.round(seconds);
  const minutes = Math.floor(rounded / 60);
  const remainder = rounded % 60;
  if (minutes >= 60) {
    const hours = Math.floor(minutes / 60);
    const mins = minutes % 60;
    return `${hours}:${String(mins).padStart(2, "0")}:${String(remainder).padStart(2, "0")}`;
  }
  return `${minutes}:${String(remainder).padStart(2, "0")}`;
}

function compactPath(path: string) {
  if (!path) return "Same as source";
  const parts = path.split(/[\\/]/).filter(Boolean);
  if (parts.length <= 2) return path;
  return `...${path.includes("\\") ? "\\" : "/"}${parts.slice(-2).join(path.includes("\\") ? "\\" : "/")}`;
}

function statusIcon(status: QueueStatus) {
  if (status === "running") return <Loader2 className="spin" size={16} />;
  if (status === "done") return <CheckCircle2 size={16} />;
  if (status === "error" || status === "canceled") return <XCircle size={16} />;
  return <Video size={16} />;
}

function App() {
  const [runtime, setRuntime] = useState<RuntimeStatus | null>(null);
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const queueRef = useRef<QueueItem[]>([]);
  const [options, setOptions] = useState<SpeedOptions>(loadSavedOptions);
  const [busy, setBusy] = useState(false);
  const [canceling, setCanceling] = useState(false);
  const [headline, setHeadline] = useState("Drop videos or folders");
  const [log, setLog] = useState<string[]>([]);
  const [dragging, setDragging] = useState(false);

  const completeCount = useMemo(() => queue.filter((item) => item.status === "done").length, [queue]);
  const errorCount = useMemo(() => queue.filter((item) => item.status === "error").length, [queue]);
  const runningItem = useMemo(() => queue.find((item) => item.status === "running"), [queue]);
  const averageSpeed = useMemo(() => {
    const speeds = queue.map((item) => item.speed).filter((speed): speed is number => typeof speed === "number");
    if (speeds.length === 0) return "--";
    return `${(speeds.reduce((sum, speed) => sum + speed, 0) / speeds.length).toFixed(2)}x`;
  }, [queue]);

  const pushLog = useCallback((line: string) => {
    setLog((current) => [line, ...current].slice(0, 80));
  }, []);

  useEffect(() => {
    queueRef.current = queue;
  }, [queue]);

  useEffect(() => {
    window.localStorage.setItem(SETTINGS_STORAGE_KEY, JSON.stringify(options));
  }, [options]);

  useEffect(() => {
    const updateScale = () => {
      const scale = Math.min(window.innerWidth / 1920, window.innerHeight / 1080);
      document.documentElement.style.setProperty("--ui-scale", String(scale));
    };
    updateScale();
    window.addEventListener("resize", updateScale);
    return () => window.removeEventListener("resize", updateScale);
  }, []);

  const refreshRuntime = useCallback(async () => {
    if (!isTauriRuntime()) {
      setRuntime({
        ffmpegFound: false,
        ffprobeFound: false,
        ffmpegPath: null,
        ffprobePath: null,
        message: "Launch with Tauri to use local videos."
      });
      return;
    }
    try {
      const status = await invoke<RuntimeStatus>("check_runtime", { ffmpegDir: options.ffmpegDir });
      setRuntime(status);
      pushLog(status.message);
    } catch (error) {
      pushLog(`Runtime check failed: ${String(error)}`);
    }
  }, [options.ffmpegDir, pushLog]);

  const probeQueuedVideos = useCallback(
    async (paths: string[]) => {
      if (!isTauriRuntime() || paths.length === 0) return;
      try {
        const infos = await invoke<VideoInfo[]>("probe_videos", { paths, ffmpegDir: options.ffmpegDir });
        const byPath = new Map(infos.map((info) => [info.path, info]));
        setQueue((current) =>
          current.map((item) => {
            const info = byPath.get(item.path);
            return info
              ? {
                  ...item,
                  durationSeconds: info.durationSeconds,
                  hasAudio: info.hasAudio,
                  message: info.error ?? item.message
                }
              : item;
          })
        );
      } catch (error) {
        pushLog(`Could not read video metadata: ${String(error)}`);
      }
    },
    [options.ffmpegDir, pushLog]
  );

  const resolvePaths = useCallback(
    async (paths: string[]) => {
      if (paths.length === 0) return;
      let resolved: string[];
      try {
        resolved = await invoke<string[]>("resolve_inputs", { paths, recursive: options.recursive });
      } catch (error) {
        pushLog(`Could not read selected paths: ${String(error)}`);
        return;
      }
      const knownBeforeAdd = new Set(queueRef.current.map((item) => item.path));
      const additions = resolved.filter((path) => !knownBeforeAdd.has(path));
      setQueue((current) => [
        ...current,
        ...additions.map((path) => ({ path, status: "pending" as const }))
      ]);
      const skipped = resolved.length - additions.length;
      setHeadline(`${additions.length} new video${additions.length === 1 ? "" : "s"} ready`);
      pushLog(
        skipped > 0
          ? `Queued ${additions.length} new video${additions.length === 1 ? "" : "s"}; ${skipped} duplicate${skipped === 1 ? "" : "s"} skipped.`
          : `Queued ${additions.length} video${additions.length === 1 ? "" : "s"}.`
      );
      void probeQueuedVideos(additions);
    },
    [options.recursive, probeQueuedVideos, pushLog]
  );

  useEffect(() => {
    void refreshRuntime();

    if (!isTauriRuntime()) {
      pushLog("Browser preview mode. Launch with Tauri to export local videos.");
      return;
    }

    const unlistenPromise = listen<WorkerEvent>("video-worker-event", (event) => {
      const payload = event.payload;
      if (payload.message && payload.type !== "log") {
        setHeadline(payload.message);
        pushLog(payload.message);
      }
      if (payload.type === "log" && payload.message) {
        pushLog(payload.message);
      }
      if (payload.type === "video_start" && payload.path) {
        setQueue((current) =>
          current.map((item) =>
            item.path === payload.path
              ? { ...item, status: "running", progress: 0, message: "Exporting" }
              : item
          )
        );
      }
      if (payload.type === "video_progress" && payload.path) {
        setQueue((current) =>
          current.map((item) =>
            item.path === payload.path
              ? { ...item, status: "running", progress: payload.percent ?? item.progress }
              : item
          )
        );
      }
      if (payload.type === "video_done" && payload.path) {
        setQueue((current) =>
          current.map((item) =>
            item.path === payload.path
              ? {
                  ...item,
                  status: "done",
                  progress: 100,
                  output: payload.output,
                  speed: payload.speed,
                  message: payload.speed ? `${payload.speed.toFixed(2)}x` : "Done"
                }
              : item
          )
        );
      }
      if (payload.type === "video_error" && payload.path) {
        setQueue((current) =>
          current.map((item) =>
            item.path === payload.path
              ? { ...item, status: "error", message: payload.message ?? "Failed" }
              : item
          )
        );
      }
      if (payload.type === "canceled") {
        setQueue((current) =>
          current.map((item) =>
            item.status === "pending" || item.status === "running"
              ? { ...item, status: "canceled", message: "Canceled" }
              : item
          )
        );
      }
      if (payload.type === "done" || payload.type === "canceled") {
        setBusy(false);
        setCanceling(false);
        void refreshRuntime();
      }
    });

    const dragPromise = getCurrentWindow().onDragDropEvent((event) => {
      if (event.payload.type === "over") {
        setDragging(true);
      }
      if (event.payload.type === "drop") {
        setDragging(false);
        pushLog(`Dropped ${event.payload.paths.length} item${event.payload.paths.length === 1 ? "" : "s"}.`);
        void resolvePaths(event.payload.paths);
      }
      if (event.payload.type === "leave") {
        setDragging(false);
      }
    });

    let aspectTimer: number | undefined;
    let aspectAdjusting = false;
    const enforceAspectRatio = async () => {
      const windowHandle = getCurrentWindow();
      const size = await windowHandle.outerSize();
      let width = Math.max(640, size.width);
      let height = Math.max(360, size.height);
      const heightFromWidth = Math.round((width * 9) / 16);
      const widthFromHeight = Math.round((height * 16) / 9);
      if (Math.abs(heightFromWidth - height) <= Math.abs(widthFromHeight - width)) {
        height = heightFromWidth;
      } else {
        width = widthFromHeight;
      }
      if (Math.abs(width - size.width) > 1 || Math.abs(height - size.height) > 1) {
        aspectAdjusting = true;
        try {
          await windowHandle.setSize(new PhysicalSize(width, height));
        } finally {
          window.setTimeout(() => {
            aspectAdjusting = false;
          }, 250);
        }
      }
    };
    const aspectPromise = getCurrentWindow().onResized(() => {
      if (aspectAdjusting) return;
      if (aspectTimer !== undefined) window.clearTimeout(aspectTimer);
      aspectTimer = window.setTimeout(() => void enforceAspectRatio(), 850);
    });

    return () => {
      if (aspectTimer !== undefined) window.clearTimeout(aspectTimer);
      void unlistenPromise.then((unlisten) => unlisten());
      void dragPromise.then((unlisten) => unlisten());
      void aspectPromise.then((unlisten) => unlisten());
    };
  }, [pushLog, refreshRuntime, resolvePaths]);

  const chooseVideos = async () => {
    if (!isTauriRuntime()) {
      pushLog("File picking requires the Tauri desktop shell.");
      return;
    }
    const selected = await open({
      multiple: true,
      directory: false,
      filters: [{ name: "Videos", extensions: VIDEO_EXTENSIONS }]
    });
    if (Array.isArray(selected)) await resolvePaths(selected);
    else if (typeof selected === "string") await resolvePaths([selected]);
  };

  const chooseFolder = async () => {
    if (!isTauriRuntime()) {
      pushLog("Folder picking requires the Tauri desktop shell.");
      return;
    }
    const selected = await open({ multiple: false, directory: true });
    if (typeof selected === "string") await resolvePaths([selected]);
  };

  const chooseOutputFolder = async () => {
    if (!isTauriRuntime()) {
      pushLog("Folder picking requires the Tauri desktop shell.");
      return;
    }
    const selected = await open({ multiple: false, directory: true });
    if (typeof selected === "string") {
      setOptions((current) => ({ ...current, outputDir: selected }));
    }
  };

  const chooseFfmpegFolder = async () => {
    if (!isTauriRuntime()) {
      pushLog("Folder picking requires the Tauri desktop shell.");
      return;
    }
    const selected = await open({ multiple: false, directory: true });
    if (typeof selected === "string") {
      setOptions((current) => ({ ...current, ffmpegDir: selected }));
    }
  };

  const reset = () => {
    if (busy) return;
    setQueue([]);
    setHeadline("Drop videos or folders");
    pushLog("Queue cleared.");
  };

  const start = async () => {
    if (!runtime?.ffmpegFound || !runtime?.ffprobeFound) {
      pushLog("ffmpeg.exe and ffprobe.exe are required before export.");
      return;
    }
    if (queue.length === 0 || busy) return;
    setBusy(true);
    setCanceling(false);
    setQueue((current) =>
      current.map((item) => ({ ...item, status: "pending", progress: 0, output: undefined, message: undefined }))
    );
    try {
      await invoke("start_speed_job", {
        paths: queue.map((item) => item.path),
        options: {
          multiplier: options.multiplier,
          useTargetLength: options.useTargetLength,
          targetSeconds: options.targetSeconds,
          stripAudio: options.stripAudio,
          replaceExisting: options.replaceExisting,
          outputFormat: options.outputFormat,
          githubGifMaxMb: options.githubGifMaxMb,
          outputDir: options.outputDir,
          ffmpegDir: options.ffmpegDir
        }
      });
      setHeadline("Export started");
    } catch (error) {
      setBusy(false);
      pushLog(`Export failed to start: ${String(error)}`);
    }
  };

  const cancelActiveJob = async () => {
    if (!busy || canceling) return;
    setCanceling(true);
    try {
      await invoke("cancel_active_job");
    } catch (error) {
      setCanceling(false);
      pushLog(`Cancel failed: ${String(error)}`);
    }
  };

  const openOutputFolder = async (path: string) => {
    try {
      await invoke("open_containing_folder", { path });
    } catch (error) {
      pushLog(`Could not open folder: ${String(error)}`);
    }
  };

  const projectedSpeed = (item: QueueItem) => {
    if (!options.useTargetLength) return `${options.multiplier.toFixed(1)}x`;
    if (!item.durationSeconds) return "--";
    return `${(item.durationSeconds / options.targetSeconds).toFixed(2)}x`;
  };

  return (
    <div className="scale-stage">
      <main className="app-shell">
        <aside className="sidebar">
          <header className="brand">
            <div className="brand-mark">
              <Gauge size={27} />
            </div>
            <div>
              <h1>BatchLapse</h1>
              <p>v{APP_VERSION}+{BUILD_HASH}</p>
            </div>
          </header>

          <section className="panel">
            <div className="panel-title">
              <Scissors size={16} />
              Export controls
            </div>

            <div className="range-field">
              <div className="range-labels">
                <span>Speed multiplier</span>
                <strong>{options.multiplier.toFixed(1)}x</strong>
              </div>
              <input
                type="range"
                min="1"
                max="10"
                step="0.1"
                disabled={options.useTargetLength}
                value={options.multiplier}
                onChange={(event) =>
                  setOptions((current) => ({ ...current, multiplier: Number(event.target.value) }))
                }
              />
            </div>

            <label className="toggle">
              <input
                type="checkbox"
                checked={options.stripAudio}
                onChange={(event) => setOptions((current) => ({ ...current, stripAudio: event.target.checked }))}
              />
              <span>Strip audio from exports</span>
            </label>

            <label className="toggle">
              <input
                type="checkbox"
                checked={options.replaceExisting}
                onChange={(event) =>
                  setOptions((current) => ({ ...current, replaceExisting: event.target.checked }))
                }
              />
              <span>Replace existing exports</span>
            </label>

            <label className="field">
              <span>Output format</span>
              <select
                value={options.outputFormat}
                onChange={(event) =>
                  setOptions((current) => ({ ...current, outputFormat: event.target.value as OutputFormat }))
                }
              >
                <option value="mp4-h264">MP4 (H.264)</option>
                <option value="webm-vp9">WebM (VP9)</option>
                <option value="github-gif">GIF (GitHub, no audio)</option>
              </select>
            </label>

            {options.outputFormat === "github-gif" ? (
              <div className="format-options">
                <div className="range-field">
                  <div className="range-labels">
                    <span>GitHub GIF size target</span>
                    <strong>{options.githubGifMaxMb.toFixed(0)} MB</strong>
                  </div>
                  <input
                    type="range"
                    min="1"
                    max="9"
                    step="1"
                    value={options.githubGifMaxMb}
                    onChange={(event) =>
                      setOptions((current) => ({ ...current, githubGifMaxMb: Number(event.target.value) }))
                    }
                  />
                </div>
                <small>GitHub GIF uploads are limited to 10 MB. BatchLapse leaves a margin and may reduce GIF size or frame rate to fit.</small>
              </div>
            ) : null}

            <label className="toggle">
              <input
                type="checkbox"
                checked={options.useTargetLength}
                onChange={(event) =>
                  setOptions((current) => ({ ...current, useTargetLength: event.target.checked }))
                }
              />
              <span>Use target video length</span>
            </label>

            {options.useTargetLength ? (
              <label className="field target-field">
                <span>Target length in seconds</span>
                <input
                  type="number"
                  min="0.1"
                  step="0.1"
                  value={options.targetSeconds}
                  onChange={(event) =>
                    setOptions((current) => ({
                      ...current,
                      targetSeconds: Math.max(0.1, Number(event.target.value) || current.targetSeconds)
                    }))
                  }
                />
              </label>
            ) : null}

            <label className="toggle">
              <input
                type="checkbox"
                checked={options.recursive}
                onChange={(event) => setOptions((current) => ({ ...current, recursive: event.target.checked }))}
              />
              <span>Scan folders recursively</span>
            </label>

            <div className="field">
              <span>Output folder</span>
              <div className="output-folder-row">
                <input
                  value={options.outputDir}
                  placeholder="Same as each source video"
                  title={options.outputDir || "Same as each source video"}
                  onChange={(event) => setOptions((current) => ({ ...current, outputDir: event.target.value }))}
                />
                <button className="secondary mini-button" onClick={chooseOutputFolder} title="Choose output folder">
                  <FolderOpen size={14} />
                </button>
                <button
                  className="secondary mini-button"
                  onClick={() => setOptions((current) => ({ ...current, outputDir: "" }))}
                  title="Use each source folder"
                >
                  Same
                </button>
              </div>
              <small>{compactPath(options.outputDir)}</small>
            </div>
          </section>

          <section className="runtime">
            <div className="panel-title">
              <Clock3 size={16} />
              Runtime
            </div>
            <p className={runtime?.ffmpegFound && runtime?.ffprobeFound ? "ok" : "warn"}>
              {runtime?.message ?? "Checking ffmpeg..."}
            </p>
            <div className="field runtime-folder">
              <span>FFmpeg folder</span>
              <div className="folder-picker-row">
                <input
                  value={options.ffmpegDir}
                  placeholder="Browse to ffmpeg bin folder"
                  title={options.ffmpegDir || "Browse to ffmpeg bin folder"}
                  onChange={(event) => setOptions((current) => ({ ...current, ffmpegDir: event.target.value }))}
                />
                <button className="secondary mini-button" onClick={chooseFfmpegFolder} title="Choose FFmpeg folder">
                  <FolderOpen size={14} />
                </button>
              </div>
            </div>
            <small>BatchLapse checks this folder, app folders, D:\Tools\ffmpeg\bin, then PATH.</small>
            <button className="secondary folder-button" onClick={() => void refreshRuntime()}>
              <RotateCcw size={16} />
              Refresh runtime
            </button>
          </section>
        </aside>

        <section className="workspace">
          <header className="topbar">
            <div>
              <p className="eyebrow">Local video batch</p>
              <h2>{headline}</h2>
            </div>
            <div className="actions">
              <button className="secondary" onClick={chooseVideos}>
                <Video size={17} />
                Videos
              </button>
              <button className="secondary" onClick={chooseFolder}>
                <FolderOpen size={17} />
                Folder
              </button>
              <button className="secondary icon" onClick={reset} disabled={busy} title="Clear queue">
                <RotateCcw size={17} />
              </button>
              <button
                className="primary"
                disabled={busy || queue.length === 0 || !runtime?.ffmpegFound || !runtime?.ffprobeFound}
                onClick={() => void start()}
              >
                {busy ? <Loader2 className="spin" size={17} /> : <Play size={17} />}
                Export
              </button>
              <button className="secondary" disabled={!busy || canceling} onClick={() => void cancelActiveJob()}>
                {canceling ? <Loader2 className="spin" size={17} /> : <Square size={17} />}
                Cancel
              </button>
            </div>
          </header>

          <div className="stats">
            <div>
              <span>{queue.length}</span>
              queued
            </div>
            <div>
              <span>{completeCount}</span>
              saved
            </div>
            <div>
              <span>{errorCount}</span>
              failed
            </div>
            <div>
              <span>{runningItem ? `${Math.round(runningItem.progress ?? 0)}%` : averageSpeed}</span>
              {runningItem ? fileName(runningItem.path) : "average speed"}
            </div>
          </div>

          <section className={`drop-zone ${dragging ? "dragging" : ""}`}>
            <Download size={42} />
            <div>
              <h3>Drop videos or folders here</h3>
              <p>
                {options.useTargetLength
                  ? `Each video is sped up to target ${options.targetSeconds}s exports.`
                  : `Each video is exported at ${options.multiplier.toFixed(1)}x speed.`}
              </p>
            </div>
          </section>

          <section className="queue">
            <div className="queue-header">
              <span>Input</span>
              <span>Duration</span>
              <span>Speed</span>
              <span>Status</span>
              <span>Output</span>
            </div>
            {queue.length === 0 ? (
              <div className="empty">
                <TriangleAlert size={18} />
                No videos queued.
              </div>
            ) : (
              queue.map((item) => (
                <div className={`queue-row ${item.status}`} key={item.path}>
                  <div className="queue-input">
                    <strong>{fileName(item.path)}</strong>
                    <small>{item.path}</small>
                  </div>
                  <div>{formatDuration(item.durationSeconds)}</div>
                  <div>{item.speed ? `${item.speed.toFixed(2)}x` : projectedSpeed(item)}</div>
                  <div className="status">
                    {statusIcon(item.status)}
                    <span>{item.message ?? item.status}</span>
                    {item.status === "running" ? (
                      <div className="progress-shell" aria-label="Export progress">
                        <div className="progress-fill" style={{ width: `${Math.round(item.progress ?? 0)}%` }} />
                      </div>
                    ) : null}
                  </div>
                  <div className="output-cell">
                    {item.output ? (
                      <>
                        <small title={item.output}>{item.output}</small>
                        <button
                          className="secondary icon output-folder"
                          onClick={() => void openOutputFolder(item.output ?? item.path)}
                          title="Open output folder"
                        >
                          <FolderOpen size={15} />
                        </button>
                      </>
                    ) : (
                      <span className="output-empty">No output yet</span>
                    )}
                  </div>
                </div>
              ))
            )}
          </section>

          <section className="log">
            {log.length === 0 ? <p>Export messages will appear here.</p> : null}
            {log.map((line, index) => (
              <p key={`${line}-${index}`}>{line}</p>
            ))}
          </section>
        </section>
      </main>
    </div>
  );
}

export default App;
