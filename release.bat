@echo off
cd /d "%~dp0src-tauri"
cargo build --release
if errorlevel 1 exit /b 1
copy /y target\release\claude-deck.exe "%~dp0claude-deck.exe" >nul
echo.
echo -^> %~dp0claude-deck.exe
