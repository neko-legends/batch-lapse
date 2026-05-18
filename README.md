# BatchLapse

BatchLapse is a local desktop batch tool for turning videos into timelapse-style
exports.

![BatchLapse screenshot](sc.webp)

## Use Cases

- Post short timelapse videos on X.
- Create animated GIFs that work well in GitHub READMEs, issues, and pull requests.
- Make a quick speed-up effect without opening a full video editor.

## GitHub GIF Example

This is a 15-second GitHub GIF export from BatchLapse:

![15-second BatchLapse GitHub GIF example](animGIF.gif)

## What It Does

- Drag and drop one video, many videos, or folders.
- Batch select videos or a folder from buttons in the toolbar.
- Speed multiplier slider from 1x to 10x.
- Strip audio checkbox, enabled by default.
- Target length mode disables the multiplier and calculates the speed from each
  source duration.
- Output formats: MP4 (H.264), WebM (VP9), or GIF for GitHub.
- GitHub GIF exports use 15 fps, loop forever, scale down to 960px wide when
  needed, and do not include audio.
- GitHub limits GIF uploads in issues and pull requests to 10 MB. BatchLapse
  caps GitHub GIF exports at 30 seconds because that is close to the upload
  limit with this profile.
- Output folder field with same-folder output as the default.
- Per-file queue status, progress, output path, and open-output-folder button.
- Existing exports are numbered automatically unless Replace existing exports is enabled.

## FFmpeg

BatchLapse requires FFmpeg. Specifically, it needs both `ffmpeg` and `ffprobe`
available on the machine. On Windows these files are named `ffmpeg.exe` and
`ffprobe.exe`.

On Windows, the portable build includes these files when they are available in
the project `bin` folder or in `D:\Tools\ffmpeg\bin`. On macOS and Linux, install
FFmpeg with your system package manager or place the binaries in the app `bin`
folder. If BatchLapse cannot find FFmpeg, use the folder button in the Runtime
panel and choose the folder that contains both files.

To download FFmpeg into this project on Windows, run this from the BatchLapse
folder:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\download-ffmpeg.ps1
```

Or download FFmpeg manually from:
[gyan.dev FFmpeg builds](https://www.gyan.dev/ffmpeg/builds/)

Yes, that PowerShell command downloads FFmpeg. It downloads the Windows
essentials build from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/), which is
linked from the official [FFmpeg download page](https://www.ffmpeg.org/download.html),
and copies `ffmpeg.exe` and `ffprobe.exe` into `bin\`.

Common macOS and Linux install commands:

```bash
# macOS with Homebrew
brew install ffmpeg

# Ubuntu or Debian
sudo apt update
sudo apt install -y ffmpeg

# Fedora
sudo dnf install ffmpeg

# Arch
sudo pacman -S ffmpeg
```

## Platform Support

The published release assets are currently Windows builds. The app code is set
up to run from source on Windows, macOS, and Linux as long as the Tauri desktop
prerequisites and FFmpeg are installed.

Use the official Tauri prerequisites page for current OS-specific system
packages:
[Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/)

## AI Agent Setup

Use this checklist when asking an AI coding agent to set up BatchLapse on a fresh
machine:

1. Install Node.js 20 or newer.
2. Install Rust stable with `rustup`.
3. Install the Tauri v2 prerequisites for the target OS.
4. Install FFmpeg and verify `ffmpeg -version` and `ffprobe -version` work.
5. Run `npm install`.
6. Run `npm run build`.
7. Run `npm run tauri:dev` for development.
8. Run `npm run tauri:build` to create platform-native Tauri bundles.

## Development

```bash
npm install
npm run build
npm run tauri:dev
```

## Windows Portable Build

```powershell
npm run tauri:build
npm run portable
```

The portable package is written to `dist-portable\`. If `bin\ffmpeg.exe` and
`bin\ffprobe.exe` exist, they are copied into the portable folder automatically.
