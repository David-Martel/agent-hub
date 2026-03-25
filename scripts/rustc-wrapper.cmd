@echo off
setlocal

set "SCCACHE=%USERPROFILE%\.cargo\bin\sccache.exe"
if exist "%SCCACHE%" goto use_sccache

for %%I in (sccache.exe) do set "SCCACHE=%%~$PATH:I"
if defined SCCACHE goto use_sccache

%*
exit /b %ERRORLEVEL%

:use_sccache
"%SCCACHE%" %*
exit /b %ERRORLEVEL%
