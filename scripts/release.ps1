[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string]$Tag = "dry-run",

    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not $DryRun -and [string]::IsNullOrWhiteSpace($Tag) -or (-not $DryRun -and $Tag -eq "dry-run")) {
    Write-Error "A release -Tag is required when not using -DryRun."
    exit 1
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    Write-Output "=== [1/4] Frontend typecheck & Vite build ==="
    Push-Location (Join-Path $repoRoot "crates\client-app")
    try {
        & npx tsc --noEmit
        if ($LASTEXITCODE -ne 0) {
            Write-Error "npx tsc --noEmit failed (exit code $LASTEXITCODE)"
            exit $LASTEXITCODE
        }
        & npm run build
        if ($LASTEXITCODE -ne 0) {
            Write-Error "npm run build failed (exit code $LASTEXITCODE)"
            exit $LASTEXITCODE
        }
    }
    finally {
        Pop-Location
    }

    Write-Output "=== [2/4] Workspace hygiene check ==="
    & .\scripts\verify-hygiene.ps1
    if ($LASTEXITCODE -ne 0) {
        Write-Error "verify-hygiene.ps1 failed (exit code $LASTEXITCODE)"
        exit $LASTEXITCODE
    }

    Write-Output "=== [3/4] Strict privacy leak scan ==="
    $bannedPatterns = @(
        "willi",
        "williamferrara",
        "100.94.",
        "100.105.",
        "100.92.",
        "100.86.",
        "100.88.",
        "10.42.0.",
        "tail4dfa1f"
    )

    $skipDirs = @("\.git\", "\target\", "\node_modules\", "\.agents\", "\crash-reports\", "\dist\")
    $skipRelFiles = @(
        "scripts\release.ps1",
        "scripts\cluster-search.ps1",
        "AGENTS.md",
        "crates\client-app\AGENTS.md",
        "crates\client-app\GEMINI.md"
    )
    $binaryExts = @(".png", ".ico", ".exe", ".dll", ".pdb", ".rlib", ".lib", ".zip", ".gz", ".lock")

    $candidateFiles = Get-ChildItem -Path $repoRoot -Recurse -File -Force | Where-Object {
        $full = $_.FullName
        $rel = $full.Substring($repoRoot.Length + 1)

        foreach ($d in $skipDirs) {
            if ($full.IndexOf($d, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
                return $false
            }
        }
        foreach ($sf in $skipRelFiles) {
            if ($rel.Equals($sf, [System.StringComparison]::OrdinalIgnoreCase)) {
                return $false
            }
        }
        if ($_.Name -like "*.bak" -or $_.Name -like "*.bak.*") {
            return $false
        }
        if ($binaryExts -contains $_.Extension.ToLowerInvariant()) {
            return $false
        }
        return $true
    }

    $leaks = [System.Collections.Generic.List[string]]::new()
    $scannedCount = 0
    foreach ($file in $candidateFiles) {
        $scannedCount++
        $rel = $file.FullName.Substring($repoRoot.Length + 1)
        $text = [System.IO.File]::ReadAllText($file.FullName, [System.Text.UTF8Encoding]::new($false))
        foreach ($pattern in $bannedPatterns) {
            if ($text.IndexOf($pattern, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
                $leaks.Add("${rel}: matched '$pattern'")
            }
        }
    }

    if ($leaks.Count -gt 0) {
        Write-Output "PRIVACY SCAN FAILED ($($leaks.Count) matches across $scannedCount files):"
        foreach ($hit in $leaks) {
            Write-Output "  LEAK: $hit"
        }
        exit 1
    }
    Write-Output "Privacy scan PASSED: 0 leaked personal strings across $scannedCount eligible files."

    if ($DryRun) {
        Write-Output "=== [4/4] DryRun mode complete for tag $Tag (no release build, commit, or push executed) ==="
        exit 0
    }

    Write-Output "=== [4/4] Building and publishing release $Tag ==="
    Get-Process -Name "duke-link-client" -ErrorAction SilentlyContinue | Stop-Process -Force

    Push-Location (Join-Path $repoRoot "crates\client-app")
    try {
        & npm run tauri -- build --bundles nsis
        if ($LASTEXITCODE -ne 0) {
            Write-Error "npm run tauri build failed (exit code $LASTEXITCODE)"
            exit $LASTEXITCODE
        }
    }
    finally {
        Pop-Location
    }

    $releaseAssets = @()
    $clientExe = Join-Path $repoRoot "target\release\duke-link-client.exe"
    if (Test-Path $clientExe) {
        $releaseAssets += $clientExe
    }
    $nsisDir = Join-Path $repoRoot "target\release\bundle\nsis"
    if (Test-Path $nsisDir) {
        Get-ChildItem -Path $nsisDir -Filter "*.exe" | ForEach-Object {
            $releaseAssets += $_.FullName
        }
    }
    if ($releaseAssets.Count -eq 0) {
        Write-Error "No release artifacts found under target\release\"
        exit 1
    }

    & git config user.email "willferrara@users.noreply.github.com"
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    & git add -A
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $status = & git status --porcelain
    if ($status) {
        & git commit -m "chore(release): prepare $Tag"
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }

    & git tag -f $Tag
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    & git push origin HEAD
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    & git push origin $Tag --force
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $prevPref = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & gh release view $Tag *> $null
    $releaseExists = ($LASTEXITCODE -eq 0)
    $ErrorActionPreference = $prevPref

    if ($releaseExists) {
        & gh release upload $Tag @releaseAssets --clobber
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    } else {
        & gh release create $Tag @releaseAssets --title "Duke Link $Tag" --generate-notes
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }

    Write-Output "=== Release $Tag published successfully ==="
    exit 0
}
finally {
    Pop-Location
}
