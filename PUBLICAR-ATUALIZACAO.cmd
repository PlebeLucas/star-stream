@echo off
chcp 65001 >nul
title STAR // STREAM - Publicar atualizacao
cd /d "%~dp0"
set "PATH=%PATH%;%ProgramFiles%\nodejs;%ProgramFiles%\Git\cmd;%ProgramFiles%\GitHub CLI;%LOCALAPPDATA%\Programs\GitHub CLI"
node setup\setup.mjs --publicar
echo.
pause
