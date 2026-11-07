$ErrorActionPreference = "Stop"

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$releaseExe = Join-Path $root "src-tauri\target\release\batchlapse.exe"
$portableDir = Join-Path $root "dist-portable"
$portableBin = Join-Path $portableDir "bin"

if (-not (Test-Path -LiteralPath $releaseExe)) {
  throw "Release executable not found. Run 'npm run tauri:build' first."
}

New-Item -ItemType Directory -Force -Path $portableDir, $portableBin | Out-Null
Copy-Item -Force -LiteralPath $releaseExe -Destination (Join-Path $portableDir "BatchLapse.exe")
Copy-Item -Force -LiteralPath (Join-Path $root "README.md") -Destination (Join-Path $portableDir "README.md")

$ffmpeg = Join-Path $root "bin\ffmpeg.exe"
$ffprobe = Join-Path $root "bin\ffprobe.exe"
if (-not ((Test-Path -LiteralPath $ffmpeg) -and (Test-Path -LiteralPath $ffprobe))) {
  # Otherwise use FFMPEG_DIR if set, else the ffmpeg on PATH.
  $ffmpegDir = $env:FFMPEG_DIR
  if (-not $ffmpegDir) {
    $onPath = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
    if ($onPath) { $ffmpegDir = Split-Path -Parent $onPath.Source }
  }
  if ($ffmpegDir) {
    $candidateFfmpeg = Join-Path $ffmpegDir "ffmpeg.exe"
    $candidateFfprobe = Join-Path $ffmpegDir "ffprobe.exe"
    if ((Test-Path -LiteralPath $candidateFfmpeg) -and (Test-Path -LiteralPath $candidateFfprobe)) {
      $ffmpeg = $candidateFfmpeg
      $ffprobe = $candidateFfprobe
    }
  }
}
if ((Test-Path -LiteralPath $ffmpeg) -and (Test-Path -LiteralPath $ffprobe)) {
  Copy-Item -Force -LiteralPath $ffmpeg -Destination (Join-Path $portableBin "ffmpeg.exe")
  Copy-Item -Force -LiteralPath $ffprobe -Destination (Join-Path $portableBin "ffprobe.exe")
}

Write-Host "Portable folder created at $portableDir"
