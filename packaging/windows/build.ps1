# Build WeaveSetup-x64.exe.
#
#     pwsh packaging/windows/build.ps1 -Binary target\release\weave.exe -OutputDir dist
#
# Stages the payload, downloads and verifies the pinned cloudflared, then hands
# everything to Inno Setup. The shared download and manifest steps are the same
# bash scripts the macOS and Linux packages use, run under Git Bash, so there is
# one pinned version and one checksum list for all three platforms.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$OutputDir,
    [string]$Repo
)

$ErrorActionPreference = 'Stop'

if (-not $Repo) {
    $Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
}
$Binary = (Resolve-Path $Binary).Path
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$OutputDir = (Resolve-Path $OutputDir).Path

$version = (Select-String -Path (Join-Path $Repo 'Cargo.toml') -Pattern '^version = "(.*)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw 'could not read the Weave version from Cargo.toml' }
Write-Output "build.ps1: packaging Weave $version"

# ---------------------------------------------------------------------------
# Stage
# ---------------------------------------------------------------------------

$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("weave-stage-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $stage | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'licenses\cloudflared') | Out-Null

try {
    Copy-Item $Binary (Join-Path $stage 'weave.exe')
    Copy-Item (Join-Path $Repo 'assets\icons\windows\weave.ico') (Join-Path $stage 'weave.ico')
    # Inno's LicenseFile wants a .txt or .rtf.
    Copy-Item (Join-Path $Repo 'LICENSE') (Join-Path $stage 'LICENSE.txt')
    Copy-Item (Join-Path $Repo 'packaging\cloudflared\licenses\cloudflared\LICENSE') `
        (Join-Path $stage 'licenses\cloudflared\LICENSE')
    Copy-Item (Join-Path $Repo 'packaging\cloudflared\licenses\cloudflared\NOTICE') `
        (Join-Path $stage 'licenses\cloudflared\NOTICE')

    # Specifically Git Bash, not whatever `bash` happens to resolve to: on most
    # Windows installs that is C:\Windows\System32\bash.exe, the WSL launcher,
    # which cannot see `/c/...` paths and fails with a confusing 127.
    $bashCandidates = @(
        "${env:ProgramFiles}\Git\bin\bash.exe",
        "${env:ProgramFiles(x86)}\Git\bin\bash.exe",
        "$env:LOCALAPPDATA\Programs\Git\bin\bash.exe"
    )
    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($git) {
        # …\Git\cmd\git.exe or …\Git\bin\git.exe -> …\Git\bin\bash.exe
        $gitRoot = Split-Path (Split-Path $git.Source -Parent) -Parent
        $bashCandidates = @((Join-Path $gitRoot 'bin\bash.exe')) + $bashCandidates
    }
    $bashPath = $bashCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $bashPath) {
        throw "Git Bash was not found (looked in: $($bashCandidates -join ', ')). Install Git for Windows."
    }
    Write-Output "build.ps1: using $bashPath"

    function Invoke-Bash {
        param([string[]]$BashArgs)
        & $bashPath @BashArgs
        if ($LASTEXITCODE -ne 0) { throw "bash $($BashArgs -join ' ') failed with $LASTEXITCODE" }
    }

    # Bash on Windows needs POSIX-style paths for its own arguments.
    function ConvertTo-BashPath {
        param([string]$WindowsPath)
        $p = $WindowsPath -replace '\\', '/'
        if ($p -match '^([A-Za-z]):(.*)$') { return "/$($Matches[1].ToLower())$($Matches[2])" }
        return $p
    }

    Invoke-Bash @(
        (ConvertTo-BashPath (Join-Path $Repo 'packaging/cloudflared/fetch.sh')),
        'cloudflared-windows-amd64.exe',
        (ConvertTo-BashPath (Join-Path $stage 'cloudflared.exe'))
    )
    Invoke-Bash @(
        (ConvertTo-BashPath (Join-Path $Repo 'packaging/bundle-manifest.sh')),
        'windows-x64-inno',
        (ConvertTo-BashPath (Join-Path $stage 'weave-bundle.json'))
    )

    # The staged pair must actually work before it is wrapped in an installer.
    & (Join-Path $stage 'cloudflared.exe') --version | Write-Output
    if ($LASTEXITCODE -ne 0) { throw 'the staged cloudflared.exe does not run' }

    # ---------------------------------------------------------------------
    # Compile
    # ---------------------------------------------------------------------

    # ISCC.exe, wherever Inno Setup 6 landed: on PATH, in either Program Files,
    # or in a per-user install. The GitHub windows runners ship it preinstalled.
    $isccPath = $null
    $onPath = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($onPath) {
        $isccPath = $onPath.Source
    } else {
        $roots = @(
            "${env:ProgramFiles(x86)}\Inno Setup 6",
            "${env:ProgramFiles}\Inno Setup 6",
            "$env:LOCALAPPDATA\Programs\Inno Setup 6"
        )
        # The uninstall entry is authoritative for a non-default location.
        foreach ($hive in @('HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
                            'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
                            'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall')) {
            $key = Join-Path $hive 'Inno Setup 6_is1'
            if (Test-Path $key) {
                $location = (Get-ItemProperty $key -ErrorAction SilentlyContinue).InstallLocation
                if ($location) { $roots = @($location) + $roots }
            }
        }
        foreach ($root in $roots) {
            $candidate = Join-Path $root 'ISCC.exe'
            if (Test-Path $candidate) { $isccPath = (Resolve-Path $candidate).Path; break }
        }
    }
    if (-not $isccPath) {
        throw @'
Inno Setup 6 was not found. Install it and re-run:

    winget install --id JRSoftware.InnoSetup --scope user

or download it from https://jrsoftware.org/isdl.php. Only ISCC.exe is needed;
a per-user install is enough and requires no administrator rights.
'@
    }
    Write-Output "build.ps1: using $isccPath"

    & $isccPath `
        "/DAppVersion=$version" `
        "/DStageDir=$stage" `
        "/DRepoDir=$Repo" `
        "/DOutputDir=$OutputDir" `
        (Join-Path $Repo 'packaging\windows\weave.iss')
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed with $LASTEXITCODE" }
} finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}

$installer = Join-Path $OutputDir 'WeaveSetup-x64.exe'
if (-not (Test-Path $installer)) { throw "expected $installer" }
Write-Output "build.ps1: $installer ($((Get-Item $installer).Length) bytes, unsigned)"
