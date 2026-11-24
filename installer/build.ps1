# Gera o instalador do OpenDesk em target\installer\OpenDesk-Setup-<versão>.exe.
#
#   powershell -ExecutionPolicy Bypass -File installer\build.ps1
#
# Compila o app em release (a não ser com -SkipBuild) e chama o compilador do
# Inno Setup 6.7 ou mais novo: o ISCC do PATH, o de $env:ISCC ou o de uma das
# pastas de instalação de sempre.

param([switch]$SkipBuild)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

$manifest = Get-Content (Join-Path $root 'Cargo.toml') -Raw
if ($manifest -notmatch '(?m)^\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"') {
    throw 'Versão não encontrada em [workspace.package] do Cargo.toml.'
}
$version = $Matches[1]

if (-not $SkipBuild) {
    cargo build --release -p opendesk --manifest-path (Join-Path $root 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'A compilação do OpenDesk falhou.' }
}

$iscc = @(
    $env:ISCC
    (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
    'C:\Tools\sdk\innosetup\ISCC.exe'
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6.7+ não encontrado. Instale-o ou aponte $env:ISCC para o ISCC.exe.' }

& $iscc /Q "/DAppVersion=$version" (Join-Path $PSScriptRoot 'OpenDesk.iss')
if ($LASTEXITCODE -ne 0) { throw 'O Inno Setup não gerou o instalador.' }

$setup = Join-Path $root "target\installer\OpenDesk-Setup-$version.exe"
Write-Host "Instalador: $setup ($([math]::Round((Get-Item $setup).Length / 1MB, 1)) MB)"
