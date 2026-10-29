@echo off
chcp 65001 >nul
title STAR // STREAM - Configurar
cd /d "%~dp0"

echo.
echo   STAR // STREAM - preparando o PC...
echo.

rem Instala o que faltar (Node.js, Git, GitHub CLI) pelo winget do Windows
where node >nul 2>nul || winget install -e --id OpenJS.NodeJS.LTS --accept-source-agreements --accept-package-agreements
where git  >nul 2>nul || winget install -e --id Git.Git --accept-source-agreements --accept-package-agreements
where gh   >nul 2>nul || winget install -e --id GitHub.cli --accept-source-agreements --accept-package-agreements

rem Programas recem-instalados ainda nao estao no PATH desta janela
set "PATH=%PATH%;%ProgramFiles%\nodejs;%ProgramFiles%\Git\cmd;%ProgramFiles%\GitHub CLI;%LOCALAPPDATA%\Programs\GitHub CLI"

node setup\setup.mjs
echo.
pause
