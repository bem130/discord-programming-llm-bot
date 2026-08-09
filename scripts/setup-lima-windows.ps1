[CmdletBinding()]
param(
    [ValidatePattern('^[A-Za-z0-9_-]+$')]
    [string] $InstanceName = 'discord-code-runner',

    [string] $Template = '',

    [switch] $SkipVmStart,

    [switch] $RecreateBrokenInstance
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $Template) {
    $Template = Join-Path $PSScriptRoot '..\src\runner\lima.yaml'
}

$limaVersion = '2.2.0'
$minimumQemuVersion = [version] '11.0.0'
$limaReleaseBaseUrl = "https://github.com/lima-vm/lima/releases/download/v$limaVersion"
$limaAssets = @{
    X64 = @{
        Name = "lima-$limaVersion-Windows-AMD64.zip"
        Sha256 = 'e7bf7dd2d2e7ab7ce020631f8f7533665f6e6f9068686d3ffcd0f3c438856ea9'
        QemuExecutable = 'qemu-system-x86_64.exe'
    }
    Arm64 = @{
        Name = "lima-$limaVersion-Windows-ARM64.zip"
        Sha256 = 'd88edbfc3a3398f5c22e2fea09de263db2dd61e3c2b0b97ed2f133bbd77d4b08'
        QemuExecutable = 'qemu-system-aarch64.exe'
    }
}

function Add-PathEntry {
    param([Parameter(Mandatory)][string] $Entry)

    $absoluteEntry = [System.IO.Path]::GetFullPath($Entry)
    $comparison = [System.StringComparison]::OrdinalIgnoreCase
    $processEntries = @($env:Path -split ';' | Where-Object { $_ })
    $processEntries = @($processEntries | Where-Object {
        -not [string]::Equals($_.TrimEnd('\'), $absoluteEntry.TrimEnd('\'), $comparison)
    })
    if ($processEntries.Count -eq 0) {
        $env:Path = $absoluteEntry
    } else {
        $env:Path = (@($absoluteEntry) + $processEntries) -join ';'
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $userEntries = @($userPath -split ';' | Where-Object { $_ })
    $userEntries = @($userEntries | Where-Object {
        -not [string]::Equals($_.TrimEnd('\'), $absoluteEntry.TrimEnd('\'), $comparison)
    })
    $newUserPath = (@($absoluteEntry) + $userEntries) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
}

function Invoke-WingetPackageCommand {
    param(
        [Parameter(Mandatory)][ValidateSet('install', 'upgrade')][string] $Action,
        [Parameter(Mandatory)][string] $Id,
        [Parameter(Mandatory)][string] $Purpose
    )

    $winget = Get-Command 'winget.exe' -ErrorAction SilentlyContinue
    if (-not $winget) {
        throw "winget is required to install or upgrade $Purpose automatically. Install it, then rerun this script."
    }

    & $winget.Source $Action --id $Id --exact --silent --accept-package-agreements `
        --accept-source-agreements --disable-interactivity
    if ($LASTEXITCODE -ne 0) {
        throw "winget failed to $Action $Purpose (package $Id)."
    }
}

function Get-QemuVersion {
    param([Parameter(Mandatory)][System.Management.Automation.CommandInfo] $Command)

    $output = (& $Command.Source --version 2>&1) -join "`n"
    if ($LASTEXITCODE -eq 0 -and $output -match 'version\s+(\d+\.\d+\.\d+)') {
        return [version] $Matches[1]
    }
    return $null
}

function Find-CompatibleQemu {
    param(
        [Parameter(Mandatory)][string] $Executable,
        [Parameter(Mandatory)][version] $MinimumVersion
    )

    $candidatePaths = @()
    $pathCommand = Get-Command $Executable -ErrorAction SilentlyContinue
    if ($pathCommand) {
        $candidatePaths += $pathCommand.Source
    }
    $candidatePaths += @('C:\qemu', 'C:\Program Files\qemu') | ForEach-Object {
        Join-Path $_ $Executable
    }

    foreach ($candidatePath in @($candidatePaths | Select-Object -Unique)) {
        if (-not (Test-Path -LiteralPath $candidatePath)) {
            continue
        }
        $candidateCommand = Get-Command $candidatePath -ErrorAction SilentlyContinue
        if (-not $candidateCommand) {
            continue
        }
        $candidateVersion = Get-QemuVersion -Command $candidateCommand
        if ($candidateVersion -and $candidateVersion -ge $MinimumVersion) {
            return [pscustomobject]@{
                Command = $candidateCommand
                Directory = Split-Path -Parent $candidateCommand.Source
                Version = $candidateVersion
            }
        }
    }
    return $null
}

function Find-GitUnixTools {
    $programFiles = [Environment]::GetFolderPath('ProgramFiles')
    $localAppData = [Environment]::GetFolderPath('LocalApplicationData')
    $candidates = @(
        (Join-Path $programFiles 'Git\usr\bin'),
        (Join-Path $localAppData 'Programs\Git\usr\bin')
    )
    foreach ($candidate in $candidates) {
        if ((Test-Path -LiteralPath (Join-Path $candidate 'gzip.exe')) -and
            (Test-Path -LiteralPath (Join-Path $candidate 'ssh.exe'))) {
            return $candidate
        }
    }
    return $null
}

$isWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows
)
if (-not $isWindows) {
    throw 'This script only supports Windows hosts.'
}

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if (-not $limaAssets.ContainsKey($architecture)) {
    throw "Unsupported Windows architecture: $architecture"
}
$asset = $limaAssets[$architecture]

$qemu = Get-Command $asset.QemuExecutable -ErrorAction SilentlyContinue
$compatibleQemu = Find-CompatibleQemu -Executable $asset.QemuExecutable `
    -MinimumVersion $minimumQemuVersion
if (-not $compatibleQemu) {
    $wingetAction = if ($qemu) { 'upgrade' } else { 'install' }
    Invoke-WingetPackageCommand -Action $wingetAction `
        -Id 'SoftwareFreedomConservancy.QEMU' -Purpose 'QEMU'
    $compatibleQemu = Find-CompatibleQemu -Executable $asset.QemuExecutable `
        -MinimumVersion $minimumQemuVersion
    if (-not $compatibleQemu) {
        throw "QEMU $minimumQemuVersion or later is required, but the detected executable is older or invalid."
    }
}
Add-PathEntry -Entry $compatibleQemu.Directory

$gitUnixTools = Find-GitUnixTools
if (-not $gitUnixTools) {
    Invoke-WingetPackageCommand -Action 'install' -Id 'Git.MinGit' `
        -Purpose 'the SSH and gzip tools required by Lima'
    $gitUnixTools = Find-GitUnixTools
}
if (-not $gitUnixTools) {
    throw 'Git Unix tools were installed but are not discoverable. Open a new PowerShell session and rerun this script.'
}
Add-PathEntry -Entry $gitUnixTools

$localAppData = [Environment]::GetFolderPath('LocalApplicationData')
$installRoot = Join-Path $localAppData "Programs\Lima\$limaVersion"
$limaExecutable = Join-Path $installRoot 'bin\limactl.exe'
if (-not (Test-Path -LiteralPath $limaExecutable)) {
    if ((Test-Path -LiteralPath $installRoot) -and
        (Get-ChildItem -LiteralPath $installRoot -Force | Select-Object -First 1)) {
        throw "The Lima install directory exists but is incomplete: $installRoot"
    }

    $downloadDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "discord-bot-lima-$limaVersion"
    New-Item -ItemType Directory -Path $downloadDirectory -Force | Out-Null
    $archive = Join-Path $downloadDirectory $asset.Name
    if (-not (Test-Path -LiteralPath $archive) -or
        (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $asset.Sha256) {
        Invoke-WebRequest -Uri "$limaReleaseBaseUrl/$($asset.Name)" -OutFile $archive
    }

    $actualHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $asset.Sha256) {
        throw "Lima archive checksum mismatch: expected $($asset.Sha256), got $actualHash"
    }

    New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
    Expand-Archive -LiteralPath $archive -DestinationPath $installRoot
}
Add-PathEntry -Entry (Join-Path $installRoot 'bin')

$reportedVersion = (& $limaExecutable --version 2>&1) -join "`n"
if ($LASTEXITCODE -ne 0 -or $reportedVersion -notmatch "\b$([regex]::Escape($limaVersion))\b") {
    throw "Unexpected Lima installation: $reportedVersion"
}

$resolvedTemplate = (Resolve-Path -LiteralPath $Template).Path
& $limaExecutable validate $resolvedTemplate
if ($LASTEXITCODE -ne 0) {
    throw "Lima rejected the template: $resolvedTemplate"
}

if (-not $SkipVmStart) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $instances = @(& $limaExecutable list --quiet 2>$null)
        $listExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($listExitCode -ne 0) {
        throw 'Failed to list Lima instances.'
    }

    $instanceExists = $instances -contains $InstanceName
    if ($instanceExists) {
        $previousErrorActionPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $instanceStatus = (& $limaExecutable list --format '{{.Status}}' $InstanceName 2>$null) -join ''
            $statusExitCode = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $previousErrorActionPreference
        }
        if ($statusExitCode -ne 0) {
            throw "Failed to inspect the Lima instance: $InstanceName"
        }
        if ($instanceStatus -eq 'Broken') {
            if (-not $RecreateBrokenInstance) {
                throw "Lima instance '$InstanceName' is broken. Rerun with -RecreateBrokenInstance to delete and recreate only this managed instance."
            }
            Write-Warning "Deleting broken Lima instance '$InstanceName' before recreating it."
            & $limaExecutable --tty=false delete --force $InstanceName
            if ($LASTEXITCODE -ne 0) {
                throw "Failed to delete the broken Lima instance: $InstanceName"
            }
            $instanceExists = $false
        }
    }

    if ($instanceExists) {
        & $limaExecutable --tty=false start $InstanceName
    } else {
        & $limaExecutable --tty=false start --name $InstanceName $resolvedTemplate
    }
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to start the Lima instance: $InstanceName"
    }

    & $limaExecutable --tty=false shell $InstanceName bash -lc `
        'set -euo pipefail
         source "$HOME/.elan/env" 2>/dev/null || true
         rustc --version
         cargo --version
         gcc --version | head -n 1
         python3 --version
         ghc --version | head -n 1
         ocaml --version
         coq --version | head -n 1
         sbcl --version | head -n 1
         elm --version
         lazyk --version 2>&1 || lazyk 2>&1 | head -n 1 || true
         lean --version'
    if ($LASTEXITCODE -ne 0) {
        throw "The Lima instance failed its toolchain smoke test: $InstanceName"
    }
}

Write-Output "Lima $limaVersion is installed and its bin directory was added to the user PATH."
if ($SkipVmStart) {
    Write-Output 'The VM start and toolchain smoke test were skipped.'
} else {
    Write-Output "Lima instance '$InstanceName' is ready."
}
Write-Output 'Open a new PowerShell session before starting the Bot so it inherits the updated PATH.'
