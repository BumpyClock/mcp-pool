[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$BinDir = (Join-Path $HOME ".local\bin")
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Remove-TemporaryFile {
    param([AllowNull()][string]$Path)

    if ($Path -and (Test-Path -LiteralPath $Path)) {
        try {
            Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
        }
        catch {
            Write-Warning "Could not remove temporary install file '$Path': $($_.Exception.Message)"
        }
    }
}

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "."))
$manifestPath = [IO.Path]::GetFullPath((Join-Path $repoRoot "Cargo.toml"))
$targetDir = [IO.Path]::GetFullPath((Join-Path $repoRoot "target"))
$requestedBinDir = if ([string]::IsNullOrWhiteSpace($BinDir)) {
    Join-Path $HOME ".local\bin"
} else {
    $BinDir
}
$resolvedBinDir = [IO.Path]::GetFullPath($requestedBinDir)
$hostTarget = $null
$sourcePath = $null
$destinationPath = [IO.Path]::GetFullPath((Join-Path $resolvedBinDir "mcp-pool.exe"))
$temporaryPath = $null

try {
    New-Item -ItemType Directory -Path $resolvedBinDir -Force | Out-Null
    $resolvedBinDir = (Get-Item -LiteralPath $resolvedBinDir).FullName
    $destinationPath = [IO.Path]::GetFullPath((Join-Path $resolvedBinDir "mcp-pool.exe"))

    $rustcVersion = (& rustc -vV 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0 -or $rustcVersion -notmatch '(?m)^host:\s*(\S+)\s*$') {
        throw "Could not determine the Rust host target; the existing executable was not changed."
    }
    $hostTarget = $Matches[1]
    $sourcePath = [IO.Path]::GetFullPath(
        (Join-Path $targetDir (Join-Path $hostTarget "release\mcp-pool.exe"))
    )

    Push-Location $repoRoot
    try {
        & cargo build --manifest-path $manifestPath --target-dir $targetDir --target $hostTarget --release --locked
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release --locked failed; the existing executable was not changed."
        }
    }
    finally {
        Pop-Location
    }

    if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
        throw "Release executable was not produced at $sourcePath; the existing executable was not changed."
    }

    $builtVersion = (& $sourcePath --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($builtVersion)) {
        throw "The release executable did not report a version; the existing executable was not changed."
    }
    Write-Output "Built version: $builtVersion"
    Write-Output "Build target: $hostTarget"
    Write-Output "Build target directory: $targetDir"

    if (Test-Path -LiteralPath $destinationPath) {
        $destinationItem = Get-Item -LiteralPath $destinationPath
        if (-not ($destinationItem -is [IO.FileInfo])) {
            throw "Install destination exists but is not a regular file: $destinationPath"
        }
        if ($destinationItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Install destination is a symbolic link or reparse point; refusing to replace it: $destinationPath"
        }
        if ($destinationItem.Attributes -band [IO.FileAttributes]::ReadOnly) {
            throw "Install destination is read-only or unwritable: $destinationPath"
        }
    }

    $temporaryPath = Join-Path $resolvedBinDir (".mcp-pool.install.$PID.$([Guid]::NewGuid().ToString('N')).tmp.exe")
    [IO.File]::Copy($sourcePath, $temporaryPath, $false)
    $sourceHash = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash
    $temporaryHash = (Get-FileHash -LiteralPath $temporaryPath -Algorithm SHA256).Hash
    if ($sourceHash -ne $temporaryHash) {
        throw "Staged executable hash did not match the build artifact; the existing executable was retained."
    }
    $stagedVersion = (& $temporaryPath --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $stagedVersion -ne $builtVersion) {
        throw "Staged executable version did not match the build artifact; the existing executable was retained."
    }

    if (Test-Path -LiteralPath $destinationPath) {
        [IO.File]::Replace($temporaryPath, $destinationPath, [NullString]::Value)
    } else {
        [IO.File]::Move($temporaryPath, $destinationPath)
    }
    $temporaryPath = $null
}
catch {
    throw "Could not install mcp-pool safely. The prior executable was retained when replacement was not completed. $($_.Exception.Message)"
}
finally {
    Remove-TemporaryFile $temporaryPath
}

$installedVersion = (& $destinationPath --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Installed executable could not report its version: $destinationPath"
}
Write-Output "Installed executable: $destinationPath"
Write-Output "Installed version: $installedVersion"

$selectedCommand = Get-Command mcp-pool -ErrorAction SilentlyContinue
if ($null -eq $selectedCommand) {
    Write-Warning "mcp-pool is not currently on PATH; no PATH changes were made."
} else {
    $selectedPath = $selectedCommand.Source
    if ([string]::IsNullOrWhiteSpace($selectedPath) -or
        ([IO.Path]::GetFullPath($selectedPath) -ne [IO.Path]::GetFullPath($destinationPath))) {
        Write-Warning "PATH mismatch: the selected mcp-pool command is '$selectedPath', not '$destinationPath'."
    }
}
