# Downloads free models/assets (Windows). Idempotent. No accounts, no keys.
param([switch]$Small)
$ErrorActionPreference = 'Continue'
Set-Location (Join-Path $PSScriptRoot '..')
New-Item -ItemType Directory -Force assets/models, assets/voices, assets/avatars, data | Out-Null
$ProgressPreference = 'SilentlyContinue'

function Set-Detail($t) { if (Test-Path data/boot) { "phase=models`nlabel=Downloading models`ndetail=$t" | Set-Content -Encoding ascii data/boot/phase.tmp; Move-Item -Force data/boot/phase.tmp data/boot/phase.txt } }

$gpu = $false
try { if (Get-Command nvidia-smi -ErrorAction SilentlyContinue) { nvidia-smi -L *> $null; if ($LASTEXITCODE -eq 0) { $gpu = $true } } } catch {}
$whisper = if ($gpu -and -not $Small) { 'ggml-base.en.bin' } else { 'ggml-tiny.en.bin' }
Write-Host "[models] GPU detected: $gpu -> whisper model: $whisper"

function Get-File($url, $dest) {
  if ((Test-Path $dest) -and ((Get-Item $dest).Length -gt 0)) { Write-Host "[models] have $dest"; return $true }
  Write-Host "[models] downloading $dest"; Set-Detail "downloading $(Split-Path $dest -Leaf)"
  try { Invoke-WebRequest -Uri $url -OutFile "$dest.part" -UseBasicParsing; Move-Item -Force "$dest.part" $dest; return $true }
  catch { Remove-Item -ErrorAction SilentlyContinue "$dest.part"; Write-Host "[models] WARNING: could not download $dest ($($_.Exception.Message))"; return $false }
}

if (-not (Test-Path "assets/models/$whisper")) {
  if (-not (Get-File "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$whisper" "assets/models/$whisper")) {
    [void](Get-File "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/$whisper" "assets/models/$whisper")
  }
}
Set-Content -NoNewline -Path assets/models/.stt-model -Value $whisper

if (-not (Test-Path assets/voices/amy)) {
  $tmp = Join-Path $env:TEMP "waifu-voice"; New-Item -ItemType Directory -Force $tmp | Out-Null
  if (Get-File "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-en_US-amy-low.tar.bz2" "$tmp/amy.tar.bz2") {
    tar -xjf "$tmp/amy.tar.bz2" -C $tmp   # tar.exe ships with Windows 10+
    Move-Item "$tmp/vits-piper-en_US-amy-low" assets/voices/amy
    Set-Content -Path assets/voices/amy/voice.yaml -Value "speaker: 0`nspeed: 1.0"
  }
}
[void](Get-File "https://raw.githubusercontent.com/vrm-c/vrm-specification/master/samples/Seed-san/vrm/Seed-san.vrm" "assets/avatars/Seed-san.vrm")

function Get-LlmSetting($key) { # value of a key inside the top-level `llm:` block of config/settings.yaml
  $f = 'config/settings.yaml'; if (-not (Test-Path $f)) { return '' }
  $in = $false
  foreach ($l in Get-Content $f) {
    if ($l -match '^llm:') { $in = $true; continue }
    if ($in -and $l -match '^[^\s#]') { break }
    if ($in -and $l -match "^\s+${key}:\s*(.*)$") { return ($Matches[1] -replace '\s*#.*$', '' -replace '["'']', '').Trim() }
  }
  return ''
}
$llm = Get-LlmSetting 'model'
if (-not $llm -or $llm -eq 'auto') { $llm = 'gemma3:4b' }
if ((Get-LlmSetting 'provider') -eq 'openai') {
  Write-Host "[models] llm.provider is openai - not pulling an Ollama model"
} elseif (Get-Command ollama -ErrorAction SilentlyContinue) {
  Write-Host "[models] ollama model: $llm"
  Set-Content -NoNewline -Path assets/models/.llm-model -Value $llm
  try { Invoke-RestMethod -Uri http://127.0.0.1:11434/api/tags -TimeoutSec 2 | Out-Null } catch {
    Set-Detail 'starting Ollama'
    Start-Process ollama -ArgumentList 'serve' -WindowStyle Hidden
    for ($i = 0; $i -lt 75; $i++) { try { Invoke-RestMethod -Uri http://127.0.0.1:11434/api/tags -TimeoutSec 1 | Out-Null; break } catch { Start-Sleep -Milliseconds 200 } }
  }
  # pull only when missing: no registry round-trip on every launch, works offline
  ollama show $llm *> $null
  if ($LASTEXITCODE -eq 0) { Write-Host "[models] have ollama model $llm" } else { Set-Detail "downloading language model $llm"; ollama pull $llm }
} else {
  Write-Host "[models] Ollama not found. Install from https://ollama.com (winget install Ollama.Ollama) for real conversations; a tiny built-in fallback brain is used meanwhile."
}
Write-Host "[models] done"
