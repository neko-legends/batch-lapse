# BatchLapse

BatchLapse is a local Windows desktop batch tool for turning videos into
timelapse-style exports.

![BatchLapse screenshot](sc.webp)

## What It Does

- Drag and drop one video, many videos, or folders.
- Batch select videos or a folder from buttons in the toolbar.
- Speed multiplier slider from 1x to 10x.
- Strip audio checkbox, enabled by default.
- Target length mode disables the multiplier and calculates the speed from each
  source duration.
- Output formats: MP4 (H.264) or WebM (VP9).
- Output folder field with same-folder output as the default.
- Per-file queue status, progress, output path, and open-output-folder button.
- Existing exports are numbered automatically unless Replace existing exports is enabled.

## FFmpeg

BatchLapse requires FFmpeg. Specifically, it needs both `ffmpeg.exe` and
`ffprobe.exe`.

The portable build includes these files when they are available in the project
`bin` folder or in `D:\Tools\ffmpeg\bin`. If BatchLapse cannot find them, use
the folder button in the Runtime panel and choose the folder that contains both
files.

To download FFmpeg into this project, run this from the BatchLapse folder:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\download-ffmpeg.ps1
```

Yes, that command downloads FFmpeg. It downloads the Windows essentials build
from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/), which is linked from the
official [FFmpeg download page](https://www.ffmpeg.org/download.html), and copies
`ffmpeg.exe` and `ffprobe.exe` into `bin\`.

## Development

```powershell
npm install
npm run build
npm run tauri:dev
```

## Portable Build

```powershell
npm run tauri:build
npm run portable
```

The portable package is written to `dist-portable\`. If `bin\ffmpeg.exe` and
`bin\ffprobe.exe` exist, they are copied into the portable folder automatically.
