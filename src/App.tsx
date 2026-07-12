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
type OutputResolution = 480 | 720 | 1080 | 1440;
type DimensionReduction = 1 | 1.5 | 2 | 2.5 | 3;
type SpeedMode = "multiplier" | "target-length";

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
  width: number | null;
  height: number | null;
  hasAudio: boolean;
  error: string | null;
};

type PreviewFrame = {
  seconds: number;
  dataUrl: string;
};

type SpeedOptions = {
  multiplier: number;
  speedMode: SpeedMode;
  targetSeconds: number;
  useClipLength: boolean;
  clipSeconds: number;
  useTargetSize: boolean;
  targetSizeMb: number;
  gifFps: number;
  stripAudio: boolean;
  replaceExisting: boolean;
  outputFormat: OutputFormat;
  outputResolution: OutputResolution;
  dimensionReduction: DimensionReduction;
  outputDir: string;
  ffmpegDir: string;
  recursive: boolean;
};

type QueueItem = {
  path: string;
  status: QueueStatus;
  durationSeconds?: number | null;
  width?: number | null;
  height?: number | null;
  hasAudio?: boolean;
  clipStartSeconds?: number;
  clipEndSeconds?: number;
  previewFrames?: PreviewFrame[];
  previewLoading?: boolean;
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

type AgentServerStatus = {
  enabled: boolean;
  port: number;
  url: string;
  openapiUrl: string;
  busy: boolean;
  activeJobId: string | null;
  message: string;
};

const SETTINGS_STORAGE_KEY = "batchlapse.settings.v1";
const AGENT_STORAGE_KEY = "batchlapse.agentControlEnabled.v1";
const AGENT_PORT_STORAGE_KEY = "batchlapse.agentApiPort.v1";
const DEFAULT_AGENT_API_PORT = 17336;
const VIDEO_EXTENSIONS = ["mp4", "mov", "m4v", "mkv", "avi", "webm", "wmv", "flv", "mpeg", "mpg", "ts", "mts", "m2ts"];
const OUTPUT_FORMAT_OPTIONS: OutputFormat[] = ["mp4-h264", "webm-vp9", "github-gif"];
const OUTPUT_RESOLUTION_OPTIONS: OutputResolution[] = [480, 720, 1080, 1440];
const DIMENSION_REDUCTION_OPTIONS: DimensionReduction[] = [1, 1.5, 2, 2.5, 3];
const SPEED_MODE_OPTIONS: SpeedMode[] = ["multiplier", "target-length"];
const GITHUB_GIF_MAX_SECONDS = 30;
const TARGET_SIZE_MAX_MB = 9.5;
const GIF_FPS_MIN = 5;
const GIF_FPS_MAX = 30;

const defaultOptions: SpeedOptions = {
  multiplier: 2,
  speedMode: "multiplier",
  targetSeconds: 10,
  useClipLength: false,
  clipSeconds: 60,
  useTargetSize: false,
  targetSizeMb: TARGET_SIZE_MAX_MB,
  gifFps: 15,
  stripAudio: true,
  replaceExisting: false,
  outputFormat: "mp4-h264",
  outputResolution: 720,
  dimensionReduction: 1,
  outputDir: "",
  ffmpegDir: "",
  recursive: true
};

const isTauriRuntime = () => "__TAURI_INTERNALS__" in window || "__TAURI__" in window;
const fileName = (path: string) => path.split(/[\\/]/).pop() ?? path;

function normalizeAgentPort(value: string) {
  const port = Number(value);
  return Number.isInteger(port) && port >= 1 && port <= 65535 ? port : null;
}

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

function coerceResolution(value: unknown, fallback: OutputResolution) {
  return OUTPUT_RESOLUTION_OPTIONS.includes(value as OutputResolution) ? (value as OutputResolution) : fallback;
}

function coerceDimensionReduction(value: unknown, fallback: DimensionReduction) {
  return DIMENSION_REDUCTION_OPTIONS.includes(value as DimensionReduction)
    ? (value as DimensionReduction)
    : fallback;
}

function loadSavedOptions(): SpeedOptions {
  if (typeof window === "undefined") return defaultOptions;
  try {
    const saved = window.localStorage.getItem(SETTINGS_STORAGE_KEY);
    if (!saved) return defaultOptions;
    const parsed = JSON.parse(saved) as Partial<SpeedOptions>;
    const outputFormat = coerceChoice(parsed.outputFormat, OUTPUT_FORMAT_OPTIONS, defaultOptions.outputFormat);
    const fallbackSpeedMode = (parsed as Partial<SpeedOptions> & { useTargetLength?: boolean }).useTargetLength
      ? "target-length"
      : defaultOptions.speedMode;
    return {
      multiplier: coerceNumber(parsed.multiplier, defaultOptions.multiplier, 1, 10),
      speedMode: coerceChoice(parsed.speedMode, SPEED_MODE_OPTIONS, fallbackSpeedMode),
      targetSeconds: coerceNumber(parsed.targetSeconds, defaultOptions.targetSeconds, 0.1, 86400),
      useClipLength: coerceBoolean(parsed.useClipLength, defaultOptions.useClipLength),
      clipSeconds: coerceNumber(parsed.clipSeconds, defaultOptions.clipSeconds, 0.1, 86400),
      useTargetSize: coerceBoolean(
        parsed.useTargetSize,
        outputFormat === "github-gif" ? true : defaultOptions.useTargetSize
      ),
      targetSizeMb: coerceNumber(parsed.targetSizeMb, defaultOptions.targetSizeMb, 0.1, TARGET_SIZE_MAX_MB),
      gifFps: coerceNumber(parsed.gifFps, defaultOptions.gifFps, GIF_FPS_MIN, GIF_FPS_MAX),
      stripAudio: coerceBoolean(parsed.stripAudio, defaultOptions.stripAudio),
      replaceExisting: coerceBoolean(parsed.replaceExisting, defaultOptions.replaceExisting),
      outputFormat,
      outputResolution: coerceResolution(parsed.outputResolution, defaultOptions.outputResolution),
      dimensionReduction: coerceDimensionReduction(parsed.dimensionReduction, defaultOptions.dimensionReduction),
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

function formatSeconds(seconds?: number | null) {
  if (typeof seconds !== "number" || !Number.isFinite(seconds)) return "--";
  if (seconds >= 60) return formatDuration(seconds);
  return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
}

function formatResolution(width?: number | null, height?: number | null) {
  if (!width || !height) return "--";
  return `${width}x${height}`;
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
  const [agentControlEnabled, setAgentControlEnabled] = useState(
    () => window.localStorage.getItem(AGENT_STORAGE_KEY) === "1"
  );
  const [agentPort, setAgentPort] = useState(
    () => window.localStorage.getItem(AGENT_PORT_STORAGE_KEY) ?? String(DEFAULT_AGENT_API_PORT)
  );
  const [agentStatus, setAgentStatus] = useState<AgentServerStatus | null>(null);

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

  const refreshAgentStatus = useCallback(async () => {
    if (!isTauriRuntime()) return;
    const status = await invoke<AgentServerStatus>("get_agent_server_status");
    setAgentStatus(status);
    if (status.enabled || !agentControlEnabled) {
      const nextPort = String(status.port || DEFAULT_AGENT_API_PORT);
      setAgentPort(nextPort);
      window.localStorage.setItem(AGENT_PORT_STORAGE_KEY, nextPort);
    }
  }, [agentControlEnabled]);

  const toggleAgentControl = useCallback(
    async (enabled: boolean) => {
      const port = normalizeAgentPort(agentPort);
      if (enabled && port === null) {
        pushLog("Choose an Agent API port between 1 and 65535.");
        return;
      }
      setAgentControlEnabled(enabled);
      window.localStorage.setItem(AGENT_STORAGE_KEY, enabled ? "1" : "0");
      if (port !== null) {
        window.localStorage.setItem(AGENT_PORT_STORAGE_KEY, String(port));
      }
      if (!isTauriRuntime()) return;
      try {
        const status = await invoke<AgentServerStatus>("set_agent_server_enabled", {
          enabled,
          port: port ?? agentStatus?.port ?? DEFAULT_AGENT_API_PORT
        });
        setAgentStatus(status);
        setAgentPort(String(status.port));
        pushLog(status.message);
      } catch (error) {
        setAgentControlEnabled(false);
        window.localStorage.setItem(AGENT_STORAGE_KEY, "0");
        pushLog(String(error));
      }
    },
    [agentPort, agentStatus?.port, pushLog]
  );

  const applyAgentPort = useCallback(async () => {
    const port = normalizeAgentPort(agentPort);
    if (port === null) {
      pushLog("Choose an Agent API port between 1 and 65535.");
      return;
    }
    window.localStorage.setItem(AGENT_PORT_STORAGE_KEY, String(port));
    if (!isTauriRuntime() || !agentControlEnabled) return;
    try {
      const status = await invoke<AgentServerStatus>("set_agent_server_enabled", {
        enabled: true,
        port
      });
      setAgentStatus(status);
      setAgentPort(String(status.port));
      pushLog(`Agent API moved to ${status.url}`);
    } catch (error) {
      pushLog(String(error));
    }
  }, [agentControlEnabled, agentPort, pushLog]);

  useEffect(() => {
    queueRef.current = queue;
  }, [queue]);

  useEffect(() => {
    window.localStorage.setItem(SETTINGS_STORAGE_KEY, JSON.stringify(options));
  }, [options]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    void refreshAgentStatus().catch((error) => pushLog(`Agent API check failed: ${String(error)}`));
  }, [pushLog, refreshAgentStatus]);

  useEffect(() => {
    if (!isTauriRuntime() || !agentControlEnabled) return;
    const port = normalizeAgentPort(agentPort) ?? DEFAULT_AGENT_API_PORT;
    void invoke<AgentServerStatus>("set_agent_server_enabled", { enabled: true, port })
      .then((status) => {
        setAgentStatus(status);
        setAgentPort(String(status.port));
      })
      .catch((error) => {
        setAgentControlEnabled(false);
        window.localStorage.setItem(AGENT_STORAGE_KEY, "0");
        pushLog(`Agent API failed to start: ${String(error)}`);
      });
  }, [agentControlEnabled, agentPort, pushLog]);

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

  const loadPreviewFrames = useCallback(
    async (paths: string[]) => {
      if (!isTauriRuntime() || paths.length === 0) return;
      for (const path of paths) {
        setQueue((current) =>
          current.map((item) => (item.path === path ? { ...item, previewLoading: true } : item))
        );
        try {
          const frames = await invoke<PreviewFrame[]>("generate_preview_frames", {
            path,
            ffmpegDir: options.ffmpegDir
          });
          setQueue((current) =>
            current.map((item) =>
              item.path === path ? { ...item, previewFrames: frames, previewLoading: false } : item
            )
          );
        } catch {
          setQueue((current) =>
            current.map((item) => (item.path === path ? { ...item, previewLoading: false } : item))
          );
        }
      }
    },
    [options.ffmpegDir]
  );

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
                  width: info.width,
                  height: info.height,
                  hasAudio: info.hasAudio,
                  clipStartSeconds: item.clipStartSeconds ?? 0,
                  clipEndSeconds: item.clipEndSeconds ?? info.durationSeconds ?? undefined,
                  message: info.error ?? item.message
                }
              : item;
          })
        );
        void loadPreviewFrames(paths);
      } catch (error) {
        pushLog(`Could not read video metadata: ${String(error)}`);
      }
    },
    [loadPreviewFrames, options.ffmpegDir, pushLog]
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
      pushLog("ffmpeg and ffprobe are required before export.");
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
        inputs: queue.map((item) => ({
          path: item.path,
          startSeconds: item.clipStartSeconds ?? 0,
          endSeconds: item.clipEndSeconds ?? item.durationSeconds ?? undefined
        })),
        options: {
          multiplier: options.multiplier,
          useTargetLength: options.speedMode === "target-length",
          targetSeconds: options.targetSeconds,
          useClipLength: options.useClipLength,
          clipSeconds: options.clipSeconds,
          useTargetSize: options.useTargetSize,
          targetSizeMb: options.targetSizeMb,
          gifFps: options.gifFps,
          stripAudio: options.stripAudio,
          replaceExisting: options.replaceExisting,
          outputFormat: options.outputFormat,
          outputResolution: options.outputResolution,
          dimensionReduction: options.dimensionReduction,
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

  const clipStart = (item: QueueItem) => item.clipStartSeconds ?? 0;

  const clipEnd = (item: QueueItem) => item.clipEndSeconds ?? item.durationSeconds ?? 0;

  const selectedSourceSeconds = (item: QueueItem) => {
    const duration = Math.max(0, clipEnd(item) - clipStart(item));
    return options.useClipLength ? Math.min(duration, options.clipSeconds) : duration;
  };

  const updateClipRange = (path: string, nextStart: number, nextEnd: number) => {
    setQueue((current) =>
      current.map((item) => {
        if (item.path !== path) return item;
        const duration = item.durationSeconds ?? 0;
        if (duration <= 0) return item;
        const start = Math.max(0, Math.min(duration, nextStart));
        const end = Math.max(start + 0.1, Math.min(duration, nextEnd));
        return {
          ...item,
          clipStartSeconds: Math.min(start, Math.max(0, end - 0.1)),
          clipEndSeconds: end
        };
      })
    );
  };

  const projectedSpeed = (item: QueueItem) => {
    if (options.speedMode !== "target-length") return `${options.multiplier.toFixed(1)}x`;
    const duration = selectedSourceSeconds(item);
    if (!duration) return "--";
    return `${(duration / options.targetSeconds).toFixed(2)}x`;
  };

  const exportSummary = options.useClipLength
    ? `Using each row's selected range, capped at ${options.clipSeconds}s.`
    : options.speedMode === "target-length"
      ? `Each selected range is sped up to target ${options.targetSeconds}s exports.`
      : `Each selected range is exported at ${options.multiplier.toFixed(1)}x speed.`;

  const renderClipControls = (item: QueueItem) => {
    const duration = item.durationSeconds ?? 0;
    const start = clipStart(item);
    const end = clipEnd(item);
    const startPercent = duration > 0 ? (start / duration) * 100 : 0;
    const endPercent = duration > 0 ? (end / duration) * 100 : 100;
    const selectedSeconds = selectedSourceSeconds(item);

    return (
      <div className="clip-cell">
        <div className="preview-strip" aria-label="Video preview strip">
          {item.previewFrames?.length ? (
            item.previewFrames.map((frame) => (
              <img src={frame.dataUrl} alt="" key={`${item.path}-${frame.seconds}`} />
            ))
          ) : (
            <div className="preview-placeholder">{item.previewLoading ? "Loading preview" : "Preview pending"}</div>
          )}
          {duration > 0 ? (
            <div
              className="range-highlight"
              style={{ left: `${startPercent}%`, width: `${Math.max(0, endPercent - startPercent)}%` }}
            />
          ) : null}
        </div>

        <div className="scrubber">
          <input
            type="range"
            min="0"
            max={duration || 0}
            step="0.1"
            value={start}
            disabled={!duration || busy}
            aria-label={`${fileName(item.path)} clip start`}
            onChange={(event) => updateClipRange(item.path, Number(event.target.value), end)}
          />
          <input
            type="range"
            min="0"
            max={duration || 0}
            step="0.1"
            value={end}
            disabled={!duration || busy}
            aria-label={`${fileName(item.path)} clip end`}
            onChange={(event) => updateClipRange(item.path, start, Number(event.target.value))}
          />
        </div>

        <div className="clip-fields">
          <label>
            <span>Start</span>
            <input
              type="number"
              min="0"
              max={duration || undefined}
              step="0.1"
              value={Number(start.toFixed(1))}
              disabled={!duration || busy}
              onChange={(event) => updateClipRange(item.path, Number(event.target.value), end)}
            />
          </label>
          <label>
            <span>End</span>
            <input
              type="number"
              min="0.1"
              max={duration || undefined}
              step="0.1"
              value={Number(end.toFixed(1))}
              disabled={!duration || busy}
              onChange={(event) => updateClipRange(item.path, start, Number(event.target.value))}
            />
          </label>
          <small>{formatSeconds(selectedSeconds)} selected</small>
        </div>
      </div>
    );
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

            <label className="field">
              <span>Output format</span>
              <select
                value={options.outputFormat}
                onChange={(event) => {
                  const outputFormat = event.target.value as OutputFormat;
                  setOptions((current) => ({
                    ...current,
                    outputFormat,
                    useTargetSize: outputFormat === "github-gif" ? true : current.useTargetSize,
                    targetSeconds: current.targetSeconds
                  }));
                }}
              >
                <option value="mp4-h264">MP4 (H.264)</option>
                <option value="webm-vp9">WebM (VP9)</option>
                <option value="github-gif">GIF (GitHub, no audio)</option>
              </select>
            </label>

            {options.outputFormat === "github-gif" ? (
              <div className="format-options">
                <small>GitHub GIF uploads are limited to 10 MB. Size target can lower FPS/resolution, but use Source clip when the export is still too long.</small>
              </div>
            ) : null}

            <label className="field">
              <span>Output resolution</span>
              <select
                value={options.outputResolution}
                onChange={(event) =>
                  setOptions((current) => ({
                    ...current,
                    outputResolution: Number(event.target.value) as OutputResolution
                  }))
                }
              >
                <option value="480">480p</option>
                <option value="720">720p</option>
                <option value="1080">1080p</option>
                <option value="1440">1440p</option>
              </select>
              <small>Preserves aspect ratio and does not upscale smaller videos.</small>
            </label>

            <label className="field">
              <span>Dimension reduction</span>
              <select
                value={options.dimensionReduction}
                onChange={(event) =>
                  setOptions((current) => ({
                    ...current,
                    dimensionReduction: Number(event.target.value) as DimensionReduction
                  }))
                }
              >
                <option value="1">Original dimensions</option>
                <option value="1.5">Reduce 1.5×</option>
                <option value="2">Reduce 2×</option>
                <option value="2.5">Reduce 2.5×</option>
                <option value="3">Reduce 3×</option>
              </select>
              <small>Divides both output dimensions after applying the resolution limit.</small>
            </label>

            <label className="field">
              <span>Timing mode</span>
              <select
                value={options.speedMode}
                onChange={(event) =>
                  setOptions((current) => ({ ...current, speedMode: event.target.value as SpeedMode }))
                }
              >
                <option value="multiplier">Speed multiplier</option>
                <option value="target-length">Target output length</option>
              </select>
            </label>

            {options.speedMode === "multiplier" ? (
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
                  value={options.multiplier}
                  onChange={(event) =>
                    setOptions((current) => ({ ...current, multiplier: Number(event.target.value) }))
                  }
                />
              </div>
            ) : (
              <label className="field target-field">
                <span>Target output length in seconds</span>
                <input
                  type="number"
                  min="0.1"
                  max={options.outputFormat === "github-gif" && !options.useTargetSize ? GITHUB_GIF_MAX_SECONDS : undefined}
                  step="0.1"
                  value={options.targetSeconds}
                  onChange={(event) => {
                    const maxSeconds =
                      options.outputFormat === "github-gif" && !options.useTargetSize ? GITHUB_GIF_MAX_SECONDS : 86400;
                    setOptions((current) => ({
                      ...current,
                      targetSeconds: Math.min(
                        maxSeconds,
                        Math.max(0.1, Number(event.target.value) || current.targetSeconds)
                      )
                    }));
                  }}
                />
              </label>
            )}

            <label className="toggle">
              <input
                type="checkbox"
                checked={options.useClipLength}
                onChange={(event) => setOptions((current) => ({ ...current, useClipLength: event.target.checked }))}
              />
              <span>Limit source clip</span>
            </label>

            {options.useClipLength ? (
              <label className="field target-field">
                <span>Use first seconds of source</span>
                <input
                  type="number"
                  min="0.1"
                  max="86400"
                  step="0.1"
                  value={options.clipSeconds}
                  onChange={(event) =>
                    setOptions((current) => ({
                      ...current,
                      clipSeconds: Math.min(86400, Math.max(0.1, Number(event.target.value) || current.clipSeconds))
                    }))
                  }
                />
              </label>
            ) : null}

            {options.outputFormat === "github-gif" ? (
              <label className="field target-field">
                <span>GIF FPS</span>
                <input
                  type="number"
                  min={GIF_FPS_MIN}
                  max={GIF_FPS_MAX}
                  step="1"
                  value={options.gifFps}
                  onChange={(event) =>
                    setOptions((current) => ({
                      ...current,
                      gifFps: Math.min(
                        GIF_FPS_MAX,
                        Math.max(GIF_FPS_MIN, Math.round(Number(event.target.value) || current.gifFps))
                      )
                    }))
                  }
                />
                <small>Size targeting may lower this automatically.</small>
              </label>
            ) : null}

            <label className="toggle">
              <input
                type="checkbox"
                checked={options.useTargetSize}
                onChange={(event) =>
                  setOptions((current) => ({
                    ...current,
                    useTargetSize: event.target.checked,
                    targetSeconds:
                      !event.target.checked && current.outputFormat === "github-gif"
                        ? Math.min(current.targetSeconds, GITHUB_GIF_MAX_SECONDS)
                        : current.targetSeconds
                  }))
                }
              />
              <span>Use target file size</span>
            </label>

            {options.useTargetSize ? (
              <label className="field target-field">
                <span>Target size in MB</span>
                <input
                  type="number"
                  min="0.1"
                  max={TARGET_SIZE_MAX_MB}
                  step="0.1"
                  value={options.targetSizeMb}
                  onChange={(event) =>
                    setOptions((current) => ({
                      ...current,
                      targetSizeMb: Math.min(
                        TARGET_SIZE_MAX_MB,
                        Math.max(0.1, Number(event.target.value) || current.targetSizeMb)
                      )
                    }))
                  }
                />
                <small>
                  MP4/WebM use bitrate targeting. GIFs lower FPS/resolution as needed to stay under the target.
                </small>
              </label>
            ) : null}

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

            <div className="agent-api-block">
              <div className="panel-title agent-title">Agent API</div>
              <label className="toggle">
                <input
                  type="checkbox"
                  checked={agentControlEnabled}
                  onChange={(event) => void toggleAgentControl(event.target.checked)}
                />
                <span>Enable local API</span>
              </label>
              <label className="field agent-port-field">
                <span>API port</span>
                <input
                  type="number"
                  min="1"
                  max="65535"
                  value={agentPort}
                  onChange={(event) => setAgentPort(event.target.value)}
                  onBlur={() => void applyAgentPort()}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.currentTarget.blur();
                    }
                  }}
                />
              </label>
              {agentStatus?.enabled ? (
                <div className="agent-status">
                  <span>{agentStatus.url}</span>
                  <small>OpenAPI: {agentStatus.openapiUrl}</small>
                </div>
              ) : null}
            </div>
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
              <p>{exportSummary}</p>
            </div>
          </section>

          <section className="queue">
            <div className="queue-header">
              <span>Input</span>
              <span>Duration</span>
              <span>Source res</span>
              <span>Clip range</span>
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
                  <div>{formatResolution(item.width, item.height)}</div>
                  {renderClipControls(item)}
                  <div>{item.speed ? `${item.speed.toFixed(2)}x` : projectedSpeed(item)}</div>
                  <div className="status">
                    {statusIcon(item.status)}
                    <span title={item.message ?? item.status}>{item.message ?? item.status}</span>
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

          <section className="diagnostics">
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
              <button className="secondary folder-button" onClick={() => void refreshRuntime()}>
                <RotateCcw size={16} />
                Refresh runtime
              </button>
            </section>

            <section className="log">
              {log.length === 0 ? <p>Export messages will appear here.</p> : null}
              {log.map((line, index) => (
                <p key={`${line}-${index}`}>{line}</p>
              ))}
            </section>
          </section>
        </section>
      </main>
    </div>
  );
}

export default App;
