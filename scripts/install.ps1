<#
.SYNOPSIS
    Explicit source-checkout compatibility publisher.

    Formal installation requires an already verified Rust package/binary.
    This wrapper never builds, downloads, or falls back to Python implicitly.
#>

[CmdletBinding()]
param(
    [string]$TargetSkillsDir = "",
    [switch]$WithPersonalSkill,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
if ($env:MISSION_CENTER_PYTHON_COMPAT -ne '1') {
    throw 'Python compatibility installer is disabled by default. Use a verified Rust package/binary for formal installation; set MISSION_CENTER_PYTHON_COMPAT=1 only for source-checkout compatibility publishing. This wrapper never builds or downloads a Rust package.'
}
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$RepoRoot = Split-Path -Parent $ScriptDir

$CodexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE '.codex' }
$PersonalSkill = if ($TargetSkillsDir) { $TargetSkillsDir } elseif ($env:MISSION_CENTER_PERSONAL_SKILL) { $env:MISSION_CENTER_PERSONAL_SKILL } else { Join-Path $CodexHome 'skills\mission-center' }
$MarketplacePlugin = if ($env:MISSION_CENTER_MARKETPLACE_PLUGIN) { $env:MISSION_CENTER_MARKETPLACE_PLUGIN } else { Join-Path $CodexHome 'local-marketplaces\mission-center\plugins\mission-center' }
$Arguments = @((Join-Path $ScriptDir 'publish_local.py'), '--repo', $RepoRoot, '--marketplace-plugin', $MarketplacePlugin, '--write')
if ($WithPersonalSkill -or $TargetSkillsDir -or $env:MISSION_CENTER_WITH_PERSONAL_SKILL -eq '1') {
    $Arguments += @('--personal-skill', $PersonalSkill)
} else {
    $Arguments += @('--remove-personal-skill', $PersonalSkill)
}
if ($env:MISSION_CENTER_RELEASE_PACKAGE) { $Arguments += @('--release-package', $env:MISSION_CENTER_RELEASE_PACKAGE) }
if ($env:MISSION_CENTER_PUBLISH_REGISTER -ne '0') { $Arguments += '--register' }
$PythonCandidates = if ($env:MISSION_CENTER_PYTHON) {
    @($env:MISSION_CENTER_PYTHON)
} elseif ($env:OS -eq 'Windows_NT') {
    @('py -3', 'python', 'python3')
} else {
    @('python3', 'python')
}
$PythonLauncher = $null
foreach ($Candidate in $PythonCandidates) {
    if (Test-Path -LiteralPath $Candidate -PathType Leaf) {
        $PythonLauncher = [pscustomobject]@{ Source = (Resolve-Path -LiteralPath $Candidate).Path; Arguments = @() }
        break
    }
    if ($Candidate -notmatch '\s') {
        $ExactCommand = Get-Command $Candidate -ErrorAction SilentlyContinue
        if ($ExactCommand) {
            $PythonLauncher = [pscustomobject]@{ Source = $ExactCommand.Source; Arguments = @() }
            break
        }
    }
    $Tokens = $null
    $ParseErrors = $null
    [void][System.Management.Automation.Language.Parser]::ParseInput("& $Candidate", [ref]$Tokens, [ref]$ParseErrors)
    $Tokens = @($Tokens | Where-Object Kind -ne EndOfInput)
    if ($ParseErrors.Count -or $Tokens.Count -lt 2) { continue }
    $CommandToken = $Tokens[1]
    $CommandName = if ($CommandToken.Value) { $CommandToken.Value } else { $CommandToken.Text }
    $PythonCommand = Get-Command $CommandName -ErrorAction SilentlyContinue
    if ($PythonCommand) {
        $PythonLauncher = [pscustomobject]@{
            Source = $PythonCommand.Source
            Arguments = @(
                $Tokens | Select-Object -Skip 2 |
                    ForEach-Object { if ($_.Kind -eq 'Parameter') { $_.Text } else { $_.Value } }
            )
        }
        break
    }
}
if (-not $PythonLauncher) { throw 'Python 3 was not found. Set MISSION_CENTER_PYTHON to its executable path.' }
$PythonArguments = @($PythonLauncher.Arguments) + $Arguments
& $PythonLauncher.Source @PythonArguments
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host 'Codex Mission Center installed successfully!' -ForegroundColor Green
