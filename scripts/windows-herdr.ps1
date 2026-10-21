# A headless herdr to drive reviewr in on Windows, dot-sourced; windows-herdr-smoke.ps1 is a journey.
# ASCII and PowerShell 5.1 only, so the VM's stock `powershell` runs it too.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# The herdr every call below runs, set by Start-HerdrServer.
$script:HerdrExe = $null
$script:HerdrServer = $null

# Download herdr's Windows release zip and return its herdr.exe. The zip carries herdr's own
# ConPTY next to the exe, so the pane runs under the console herdr ships with.
function Install-Herdr {
    param(
        [string] $Version = '0.9.3',
        [string] $Dest = (Join-Path ([IO.Path]::GetTempPath()) "herdr-$Version")
    )
    $exe = Join-Path $Dest 'herdr.exe'
    if (-not (Test-Path -LiteralPath $exe)) {
        [Net.ServicePointManager]::SecurityProtocol =
            [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $zip = Join-Path ([IO.Path]::GetTempPath()) "herdr-$Version-windows-x86_64.zip"
        $url = "https://github.com/ogulcancelik/herdr/releases/download/v$Version/herdr-windows-x86_64.zip"
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
        Expand-Archive -LiteralPath $zip -DestinationPath $Dest -Force
    }
    return $exe
}

# Run herdr and return stdout and stderr as one string, throwing on a non-zero exit alone. No param
# block, so herdr's `--flags` pass through as arguments.
function Invoke-Herdr {
    $Arguments = [string[]] $args
    $ErrorActionPreference = 'Continue'
    $output = & $script:HerdrExe @Arguments 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $text = $output -join "`n"
    if ($code -ne 0) {
        throw "herdr $($Arguments -join ' ') exited $code`: $text"
    }
    return $text
}

# Start a herdr server in a private session (HERDR_SESSION), so it shares nothing with any other
# herdr on the machine, and wait until it answers.
function Start-HerdrServer {
    param(
        [Parameter(Mandatory = $true)] [string] $Herdr,
        [string] $Session = "reviewr-ci-$([guid]::NewGuid().ToString('N'))"
    )
    $script:HerdrExe = (Resolve-Path -LiteralPath $Herdr).Path
    $env:HERDR_SESSION = $Session
    Remove-Item Env:HERDR_SOCKET_PATH, Env:HERDR_CLIENT_SOCKET_PATH -ErrorAction SilentlyContinue
    $script:HerdrServer = Start-Process -FilePath $script:HerdrExe -ArgumentList 'server' -PassThru -WindowStyle Hidden
    Wait-For 'the herdr server' -TimeoutSec 15 {
        try { (Invoke-Herdr status server) -match 'status: running' } catch { $false }
    } | Out-Null
}

# Stop the server Start-HerdrServer started, killing its process tree if it lingers.
function Stop-HerdrServer {
    if ($null -eq $script:HerdrServer) { return }
    try { Invoke-Herdr server stop | Out-Null } catch { Write-Host "server stop: $($_.Exception.Message)" }
    $null = $script:HerdrServer.WaitForExit(10000)
    $script:HerdrServer.Refresh()
    if (-not $script:HerdrServer.HasExited) {
        & taskkill.exe /PID $script:HerdrServer.Id /T /F 2>&1 | Out-Null
    }
    $script:HerdrServer = $null
    $global:LASTEXITCODE = 0
}

# Poll `Condition` until it returns something truthy, and return that. Throws naming `What`
# once `TimeoutSec` passes.
function Wait-For {
    param(
        [Parameter(Mandatory = $true, Position = 0)] [string] $What,
        [Parameter(Mandatory = $true, Position = 1)] [scriptblock] $Condition,
        [int] $TimeoutSec = 30
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ($true) {
        $result = & $Condition
        if ($result) { return $result }
        if ((Get-Date) -ge $deadline) { throw "timed out after ${TimeoutSec}s waiting for $What" }
        Start-Sleep -Milliseconds 250
    }
}

# Invoke plugin action `Action` as a keypress does, and return its finished run from herdr's
# plugin log: .status, .exit_code, .stdout, .stderr.
function Invoke-PluginAction {
    param(
        [Parameter(Mandatory = $true, Position = 0)] [string] $Action,
        [string] $Plugin = 'kaishin.reviewr',
        [int] $TimeoutSec = 30
    )
    $invoked = (Invoke-Herdr plugin action invoke $Action --plugin $Plugin) | ConvertFrom-Json
    $logId = $invoked.result.log.log_id
    return Wait-For "action $Action ($logId) to finish" -TimeoutSec $TimeoutSec {
        ((Invoke-Herdr plugin log list --plugin $Plugin) | ConvertFrom-Json).result.logs |
            Where-Object { $_.log_id -eq $logId -and $_.status -ne 'running' }
    }
}

# Create a focused workspace whose root pane starts in `Cwd`. Returns .Workspace and .Pane.
function New-HerdrWorkspace {
    param([Parameter(Mandatory = $true)] [string] $Cwd)
    $created = (Invoke-Herdr workspace create --cwd $Cwd --focus) | ConvertFrom-Json
    return [pscustomobject]@{
        Workspace = $created.result.workspace.workspace_id
        Pane      = $created.result.root_pane.pane_id
    }
}

# The pane ids in `Workspace`, in listing order.
function Get-PaneIds {
    param([Parameter(Mandatory = $true)] [string] $Workspace)
    return @(((Invoke-Herdr pane list --workspace $Workspace) | ConvertFrom-Json).result.panes |
        ForEach-Object { $_.pane_id })
}

# The visible screen of `Pane`, as text.
function Read-Pane {
    param([Parameter(Mandatory = $true)] [string] $Pane)
    return Invoke-Herdr pane read $Pane --source visible --format text
}

# Wait until the visible screen of `Pane` matches the regex `Pattern`, and return the screen.
# On a timeout the error carries the last screen read.
function Wait-PaneText {
    param(
        [Parameter(Mandatory = $true, Position = 0)] [string] $Pane,
        [Parameter(Mandatory = $true, Position = 1)] [string] $Pattern,
        [int] $TimeoutSec = 30
    )
    $script:LastScreen = ''
    try {
        return Wait-For "pane $Pane to show '$Pattern'" -TimeoutSec $TimeoutSec {
            try { $script:LastScreen = Read-Pane $Pane } catch { $script:LastScreen = "$($_.Exception.Message)" }
            if ($script:LastScreen -match $Pattern) { $script:LastScreen }
        }
    } catch {
        throw "$($_.Exception.Message). Last screen:`n$script:LastScreen"
    }
}

# Send logical keys (`Enter`, `Esc`, `j`, ...) to `Pane`.
function Send-PaneKeys {
    param(
        [Parameter(Mandatory = $true, Position = 0)] [string] $Pane,
        [Parameter(ValueFromRemainingArguments = $true)] [string[]] $Keys
    )
    Invoke-Herdr pane send-keys $Pane @Keys | Out-Null
}

# Write `Text` into `Pane`'s input literally, without submitting it.
function Send-PaneText {
    param(
        [Parameter(Mandatory = $true, Position = 0)] [string] $Pane,
        [Parameter(Mandatory = $true, Position = 1)] [string] $Text
    )
    Invoke-Herdr pane send-text $Pane $Text | Out-Null
}
