@echo off
rem dockur runs this as SYSTEM once, right after Windows install (no UAC).
rem Decouple the long provisioning into a SYSTEM scheduled task so it survives
rem oem-context teardown and any dockur timeout on this script.
copy /y "%~dp0setup.ps1" C:\setup.ps1 >nul
schtasks /create /tn "TriremeSetup" /tr "powershell -ExecutionPolicy Bypass -NoProfile -File C:\setup.ps1" /sc onstart /ru SYSTEM /rl HIGHEST /f
schtasks /run /tn "TriremeSetup"
exit /b 0
