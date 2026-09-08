[CmdletBinding()]
param([switch]$WithPersonalSkill)

$ErrorActionPreference = "Stop"
if ($env:MISSION_CENTER_PYTHON_COMPAT -ne "1") {
  throw "Python compatibility installer is disabled by default. Use a verified Rust package/binary for formal installation; set MISSION_CENTER_PYTHON_COMPAT=1 only for source-checkout compatibility publishing. This wrapper never builds or downloads a Rust package."
}

$root = Split-Path -Parent $PSScriptRoot
$codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE ".codex" }
$personalSkill = if ($env:MISSION_CENTER_PERSONAL_SKILL) {
  $env:MISSION_CENTER_PERSONAL_SKILL
} else {
  Join-Path $codexHome "skills\mission-center"
}
$marketplacePlugin = if ($env:MISSION_CENTER_MARKETPLACE_PLUGIN) {
  $env:MISSION_CENTER_MARKETPLACE_PLUGIN
} else {
  Join-Path $codexHome "local-marketplaces\mission-center\plugins\mission-center"
}
$mode = if ($env:MISSION_CENTER_PUBLISH_MODE) { $env:MISSION_CENTER_PUBLISH_MODE } else { "--write" }
if ($mode -notin @("--dry-run", "--write", "--verify")) {
  throw "MISSION_CENTER_PUBLISH_MODE must be --dry-run, --write, or --verify"
}

$pythonCandidates = if ($env:MISSION_CENTER_PYTHON) { @($env:MISSION_CENTER_PYTHON) } else { @("py -3", "python", "python3") }
$pythonLauncher = $null
foreach ($candidate in $pythonCandidates) {
  if (Test-Path -LiteralPath $candidate -PathType Leaf) {
    $pythonLauncher = [pscustomobject]@{ Source = (Resolve-Path -LiteralPath $candidate).Path; Arguments = @() }
    break
  }
  if ($candidate -notmatch '\s') {
    $exactCommand = Get-Command $candidate -ErrorAction SilentlyContinue
    if ($exactCommand) {
      $pythonLauncher = [pscustomobject]@{ Source = $exactCommand.Source; Arguments = @() }
      break
    }
  }
  $tokens = $null
  $parseErrors = $null
  [void][System.Management.Automation.Language.Parser]::ParseInput("& $candidate", [ref]$tokens, [ref]$parseErrors)
  $tokens = @($tokens | Where-Object Kind -ne EndOfInput)
  if ($parseErrors.Count -or $tokens.Count -lt 2) { continue }
  $commandToken = $tokens[1]
  $commandName = if ($commandToken.Value) { $commandToken.Value } else { $commandToken.Text }
  $pythonCommand = Get-Command $commandName -ErrorAction SilentlyContinue
  if ($pythonCommand) {
    $pythonLauncher = [pscustomobject]@{
      Source = $pythonCommand.Source
      Arguments = @(
        $tokens | Select-Object -Skip 2 |
          ForEach-Object { if ($_.Kind -eq "Parameter") { $_.Text } else { $_.Value } }
      )
    }
    break
  }
}
if (-not $pythonLauncher) { throw "Python 3 was not found. Set MISSION_CENTER_PYTHON to its executable path." }

$pythonArguments = @($pythonLauncher.Arguments) + @(
  (Join-Path $PSScriptRoot "publish_local.py")
  "--repo"
  $root
  "--marketplace-plugin"
  $marketplacePlugin
  $mode
)
if ($WithPersonalSkill -or $env:MISSION_CENTER_WITH_PERSONAL_SKILL -eq "1") {
  $pythonArguments += @("--personal-skill", $personalSkill)
} else {
  $pythonArguments += @("--remove-personal-skill", $personalSkill)
}
if ($env:MISSION_CENTER_RELEASE_PACKAGE) {
  $pythonArguments += @("--release-package", $env:MISSION_CENTER_RELEASE_PACKAGE)
}
if ($mode -eq "--write") { $pythonArguments += "--register" }
& $pythonLauncher.Source @pythonArguments
if ($LASTEXITCODE -ne 0) {
  exit $LASTEXITCODE
}

switch ($mode) {
  "--dry-run" { Write-Output "Dry-run completed. No files were modified." }
  "--write" { Write-Output "Published Mission Center local marketplace plugin and refreshed Codex plugin registration." }
  "--verify" { Write-Output "Verification completed successfully." }
}
