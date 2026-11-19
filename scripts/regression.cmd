@echo off
rem Run the duthub regression checks with Git Bash.
where bash >nul 2>&1
if errorlevel 1 (
  echo Git Bash is required. Put bash on PATH and try again.
  exit /b 1
)
bash "%~dp0regression" %*
exit /b %errorlevel%
