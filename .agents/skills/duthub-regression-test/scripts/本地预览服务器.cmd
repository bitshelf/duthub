@echo off
setlocal
pushd "%~dp0..\..\..\.." || exit /b 1
set "PYTHONUNBUFFERED=1"
echo Open http://127.0.0.1:8765/duthub.html after the preview server starts.
py -3 --version >nul 2>&1
if not errorlevel 1 (
  py -3 -m http.server 8765 --bind 127.0.0.1 --directory docs
  goto :finish
)
python --version >nul 2>&1
if not errorlevel 1 (
  python -m http.server 8765 --bind 127.0.0.1 --directory docs
  goto :finish
)
echo Python 3 is required for the archived HTML prototype. 1>&2
popd
exit /b 1

:finish
set "result=%errorlevel%"
popd
exit /b %result%
