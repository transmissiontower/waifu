@echo off
setlocal EnableDelayedExpansion
rem One-command launch (Windows): installs Rust + build tools if missing, builds in release mode,
rem downloads models, starts every service under the supervisor and opens the avatar window.
rem No Docker, no API keys, no accounts.     Usage: run.bat [--no-open] [--small]
cd /d "%~dp0"
set "OPEN=--open"
set "SMALL="
for %%A in (%*) do (
  if /I "%%A"=="--no-open" set "OPEN="
  if /I "%%A"=="--small" set "SMALL=-Small"
)
set "PATH=%USERPROFILE%\.cargo\bin;%ProgramFiles%\CMake\bin;%ProgramFiles%\LLVM\bin;%PATH%"

rem ---- 1. Visual Studio C++ build tools (MSVC linker) ----
where link.exe >nul 2>nul
if errorlevel 1 (
  set "VSW=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
  set "HAVEVS="
  if exist "!VSW!" for /f "usebackq tokens=*" %%i in (`"!VSW!" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "HAVEVS=%%i"
  if not defined HAVEVS (
    echo [run] installing Visual Studio Build Tools ^(C++^) - this is a one-time ~3 GB install...
    winget install -e --id Microsoft.VisualStudio.2022.BuildTools --accept-package-agreements --accept-source-agreements --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
  )
)

rem ---- 2. cmake + LLVM (whisper.cpp and bindgen) ----
where cmake >nul 2>nul || (echo [run] installing CMake & winget install -e --id Kitware.CMake --accept-package-agreements --accept-source-agreements)
where clang >nul 2>nul || (echo [run] installing LLVM & winget install -e --id LLVM.LLVM --accept-package-agreements --accept-source-agreements)
set "PATH=%ProgramFiles%\CMake\bin;%ProgramFiles%\LLVM\bin;%PATH%"
set "LIBCLANG_PATH=%ProgramFiles%\LLVM\bin"

rem ---- 3. Rust via rustup ----
where cargo >nul 2>nul
if errorlevel 1 (
  echo [run] installing Rust toolchain with rustup...
  powershell -NoProfile -Command "Invoke-WebRequest https://win.rustup.rs/x86_64 -OutFile rustup-init.exe -UseBasicParsing"
  rustup-init.exe -y --profile minimal
  del rustup-init.exe
  set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
)
cargo --version || (echo [run] Rust install failed & exit /b 1)

rem ---- 4. loading screen: tiny dependency-free server, alive while everything below compiles/downloads/starts ----
rem      (optional: if it cannot be built everything works as before, without the loading screen)
set "BOOT=data\boot"
if exist "%BOOT%" rmdir /s /q "%BOOT%"
mkdir "%BOOT%"
set "LAUNCHER="
cargo build --release -p launcher >nul 2>"%BOOT%\launcher-build.log"
if errorlevel 1 (
  echo [run] loading screen unavailable ^(see %BOOT%\launcher-build.log^) - continuing without it
) else (
  set "LAUNCHER=1"
  start "" /B "target\release\launcher.exe" --root . --dir "%BOOT%" %OPEN%
)

rem ---- 5. build ----
call :phase build "Compiling the engine"
echo [run] building release ^(first build takes a while: whisper.cpp + sherpa-onnx^)...
cargo build --release --workspace --exclude e2e --exclude launcher > "%BOOT%\build.log" 2>&1
if errorlevel 1 (
  type "%BOOT%\build.log"
  call :fail build "Compiling the engine" "The build failed - see the log"
  exit /b 1
)

rem sherpa-onnx / onnxruntime DLLs must sit next to the executables
powershell -NoProfile -Command "Get-ChildItem -Recurse -Path target\release\build,$env:LOCALAPPDATA\sherpa-rs -Include onnxruntime*.dll,sherpa-onnx*.dll -ErrorAction SilentlyContinue | ForEach-Object { Copy-Item $_.FullName target\release\ -Force }"

rem ---- 6. models & assets ----
call :phase models "Downloading models"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\download-models.ps1 %SMALL%

rem ---- 7. frontend (rebuild when the bundle is missing) ----
where npm >nul 2>nul && if exist frontend\package.json if not exist frontend\dist\app.js (
  call :phase frontend "Building the interface"
  pushd frontend & call npm install --no-audit --no-fund & call npm run build & popd
)

rem ---- 8. go ----
rem with a loading screen it opens the window itself and hands over to the app when everything is ready
call :phase services "Starting services"
echo [run] starting - Ctrl+C stops everything
if defined LAUNCHER (set "OPEN=")
target\release\supervisor.exe %OPEN%
taskkill /F /IM launcher.exe >nul 2>nul
endlocal
exit /b 0

:phase
(
  echo phase=%~1
  echo label=%~2
  echo detail=%~3
) > "%BOOT%\phase.tmp"
move /y "%BOOT%\phase.tmp" "%BOOT%\phase.txt" >nul
exit /b 0

:fail
(
  echo phase=%~1
  echo label=%~2
  echo error=%~3
) > "%BOOT%\phase.tmp"
move /y "%BOOT%\phase.tmp" "%BOOT%\phase.txt" >nul
timeout /t 30 >nul
taskkill /F /IM launcher.exe >nul 2>nul
exit /b 0
