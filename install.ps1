[CmdletBinding()]
param(
    [string]$Repo = $(if ($env:ORCA_REPO) { $env:ORCA_REPO } else { "echoVic/orca-agent" }),
    [string]$Version = $(if ($env:ORCA_VERSION) { $env:ORCA_VERSION } else { "latest" }),
    [string]$InstallDir = $(if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\Orca\bin" }),
    [int]$WaitForPid = 0,
    [switch]$SetupSandbox,
    [switch]$RepairSandbox,
    [switch]$RemoveSandbox,
    [switch]$NonInteractive
)

$ErrorActionPreference = "Stop"

function Get-OrcaTarget {
    $architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
    switch ($architecture) {
        "x64" { return "x86_64-pc-windows-msvc" }
        "arm64" { return "aarch64-pc-windows-msvc" }
        default { throw "Unsupported Windows architecture: $architecture" }
    }
}

function Get-ReleasePath([string]$RequestedVersion) {
    if ($RequestedVersion -eq "latest") {
        return "latest/download"
    }
    if ($RequestedVersion.StartsWith("v")) {
        return "download/$RequestedVersion"
    }
    return "download/v$RequestedVersion"
}

function Install-OrcaBinary([string]$Source, [string]$Destination) {
    $backup = "$Destination.previous"
    Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $Destination) {
        Move-Item -LiteralPath $Destination -Destination $backup -Force
    }
    try {
        Move-Item -LiteralPath $Source -Destination $Destination -Force
        Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    }
    catch {
        if ((Test-Path -LiteralPath $backup) -and -not (Test-Path -LiteralPath $Destination)) {
            Move-Item -LiteralPath $backup -Destination $Destination -Force
        }
        throw
    }
}

if ($WaitForPid -gt 0) {
    Wait-Process -Id $WaitForPid -ErrorAction SilentlyContinue
}

if (($SetupSandbox -and $RepairSandbox) -or ($SetupSandbox -and $RemoveSandbox) -or ($RepairSandbox -and $RemoveSandbox)) {
    throw "SetupSandbox, RepairSandbox, and RemoveSandbox are mutually exclusive"
}

$orcaHome = if ($env:ORCA_HOME) { $env:ORCA_HOME } else { Join-Path $env:USERPROFILE ".orca" }
$stateDir = Join-Path $orcaHome "windows-capabilities"
$setupHelperPath = Join-Path $InstallDir "orca-windows-sandbox-setup.exe"

if ($RemoveSandbox) {
    if (-not (Test-Path -LiteralPath $setupHelperPath -PathType Leaf)) {
        Write-Host "Windows sandbox setup helper is not installed; nothing to remove."
        return
    }
    if (-not (Test-Path -LiteralPath $stateDir -PathType Container)) {
        Write-Host "Windows sandbox state is absent; nothing to remove."
        return
    }
    $removeRequest = @{
        version = 1
        operation = "remove"
        state_dir = $stateDir
        workspace = (Get-Location).Path
    } | ConvertTo-Json -Compress
    $removeResponse = $removeRequest | & $setupHelperPath
    if ($LASTEXITCODE -ne 0) {
        throw "Windows sandbox removal failed: $removeResponse"
    }
    $parsedRemoveResponse = $removeResponse | ConvertFrom-Json
    if (-not $parsedRemoveResponse.ok) {
        throw "Windows sandbox removal failed: $($parsedRemoveResponse.error)"
    }
    if ($parsedRemoveResponse.removed) {
        Write-Host "Windows sandbox capability removed for $((Get-Location).Path)"
    } else {
        Write-Host "Windows sandbox capability was already absent for $((Get-Location).Path)"
    }
    return
}

$target = Get-OrcaTarget
$archiveName = "orca-$target.zip"
$checksumName = "$archiveName.sha256"
$releasePath = Get-ReleasePath $Version
$baseUrl = "https://github.com/$Repo/releases/$releasePath"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("orca-install-" + [guid]::NewGuid().ToString("N"))
$archivePath = Join-Path $tempDir $archiveName
$checksumPath = Join-Path $tempDir $checksumName
$extractDir = Join-Path $tempDir "extract"

New-Item -ItemType Directory -Path $tempDir | Out-Null
try {
    Write-Host "Installing Orca for $target"
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$archiveName" -OutFile $archivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$checksumName" -OutFile $checksumPath

    $expected = ((Get-Content -LiteralPath $checksumPath -Raw).Trim() -split "\s+")[0].ToLowerInvariant()
    if ($expected -notmatch "^[0-9a-f]{64}$") {
        throw "Invalid checksum file for $archiveName"
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "Checksum mismatch for $archiveName (expected $expected, got $actual)"
    }

    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force
    $bundleMembers = @(
        "orca.exe",
        "orca-windows-runner.exe",
        "orca-windows-sandbox-setup.exe",
        "LICENSE"
    )
    foreach ($member in $bundleMembers) {
        $memberPath = Join-Path $extractDir $member
        if (-not (Test-Path -LiteralPath $memberPath -PathType Leaf)) {
            throw "$archiveName did not contain required bundle member $member"
        }
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    foreach ($member in $bundleMembers) {
        $sourcePath = Join-Path $extractDir $member
        $destinationPath = Join-Path $InstallDir $member
        Install-OrcaBinary $sourcePath $destinationPath
    }
    $installPath = Join-Path $InstallDir "orca.exe"
    Write-Host "Installed $installPath and native Windows helpers"

    if ($SetupSandbox -or $RepairSandbox) {
        $stateDir = Join-Path $orcaHome "windows-capabilities"
        New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
        $setupRequest = @{
            version = 1
            operation = if ($RepairSandbox) { "repair" } else { "provision" }
            state_dir = $stateDir
            workspace = (Get-Location).Path
        } | ConvertTo-Json -Compress
        $setupResponse = $setupRequest | & $setupHelperPath
        if ($LASTEXITCODE -ne 0) {
            throw "Windows sandbox setup failed: $setupResponse"
        }
        $parsedSetupResponse = $setupResponse | ConvertFrom-Json
        if (-not $parsedSetupResponse.ok) {
            throw "Windows sandbox setup failed: $($parsedSetupResponse.error)"
        }
        Write-Host "Windows sandbox capability receipt ready in $stateDir"
    }

    $pathEntries = $env:PATH -split ";"
    if ($pathEntries -notcontains $InstallDir) {
        Write-Host "Add $InstallDir to PATH to run orca from any directory."
    }
    & $installPath --version
}
finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
