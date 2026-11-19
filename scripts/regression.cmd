@echo off
rem duthub 统一回归门槛（Git Bash 下运行 scripts/regression）
where bash >/dev/null 2>&1 && bash "%~dp0regression" || (
  echo 需要 Git Bash（bash 不在 PATH）
  exit /b 1
)
