# Toggle reviewr open and closed in a headless Windows herdr, and prove an idle pane sits still:
# powershell -NoProfile -ExecutionPolicy Bypass -File <this> -Reviewr <exe>
param(
    [Parameter(Mandatory = $true)] [string] $Reviewr
)

. (Join-Path $PSScriptRoot 'windows-herdr.ps1')

# The log lines written while `$during` runs.
function Get-LogWindow([string] $Path, [scriptblock] $During) {
    $before = @(Get-Content -LiteralPath $Path -ErrorAction SilentlyContinue).Count
    & $During
    return @(Get-Content -LiteralPath $Path | Select-Object -Skip $before)
}

$checkout = Split-Path -Parent $PSScriptRoot
$plugin = 'kaishin.reviewr'

New-Item -ItemType Directory -Force -Path (Join-Path $checkout 'bin') | Out-Null
Copy-Item -LiteralPath $Reviewr -Destination (Join-Path $checkout 'bin\herdr-reviewr.exe') -Force

# A repo with one untracked file: reviewr's file list shows its name, which only a painted
# reviewr frame can put on the screen.
$marker = 'reviewr-smoke-marker.txt'
$repo = Join-Path ([IO.Path]::GetTempPath()) "reviewr-smoke-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $repo | Out-Null
foreach ($gitArgs in @(
        @('init', '-q'),
        @('-c', 'user.name=ci', '-c', 'user.email=ci@example.com', 'commit', '-q', '--allow-empty', '-m', 'init')
    )) {
    & git -C $repo @gitArgs
    if ($LASTEXITCODE -ne 0) { throw "git $($gitArgs -join ' ') failed" }
}
Set-Content -LiteralPath (Join-Path $repo $marker) -Value 'hello' -Encoding ascii

# The manifest's floor, so the oldest herdr it admits is the one proven.
$manifest = Get-Content -LiteralPath (Join-Path $checkout 'herdr-plugin.toml') -Raw
$floor = [regex]::Match($manifest, '(?m)^min_herdr_version = "([^"]+)"').Groups[1].Value
$herdr = Install-Herdr -Version $floor
# The pane inherits the server's environment, so reviewr's event log lands here.
$log = Join-Path ([IO.Path]::GetTempPath()) "reviewr-smoke-$([guid]::NewGuid().ToString('N')).log"
$env:HERDR_REVIEW_LOG = $log
Start-HerdrServer -Herdr $herdr
try {
    Invoke-Herdr plugin link $checkout | Out-Null
    $ws = New-HerdrWorkspace -Cwd $repo

    # Each toggle runs to completion before the next, as two keypresses a moment apart do.
    $open = Invoke-PluginAction toggle -Plugin $plugin
    Write-Host "toggle: exit $($open.exit_code): $($open.stdout)$($open.stderr)"
    $pane = @(Get-PaneIds $ws.Workspace | Where-Object { $_ -ne $ws.Pane })
    if ($open.exit_code -ne 0 -or $pane.Count -ne 1 -or $open.stdout -notmatch "^reviewr: opened $($pane[0]) ") {
        throw "the first toggle did not open one reviewr pane (panes: $($pane -join ', '))"
    }
    $pane = $pane[0]
    Wait-PaneText $pane ([regex]::Escape($marker)) | Write-Host
    Write-Host (Invoke-Herdr pane process-info --pane $pane)

    # Quiet: with nothing changing, the loop wakes for nothing and starts no process.
    Start-Sleep -Seconds 3
    $quiet = Get-LogWindow $log { Start-Sleep -Seconds 10 }
    $busy = @($quiet | Where-Object { $_ -match ' (wake|spawn) ' })
    Write-Host "quiet 10s: $($busy.Count) wakes or process starts"
    if ($busy.Count -ne 0) { throw "an idle pane woke: $($busy -join '; ')" }

    # An edit reaches the pane through the watcher.
    $edit = Get-LogWindow $log {
        Add-Content -LiteralPath (Join-Path $repo $marker) -Value 'edited'
        Start-Sleep -Seconds 2
    }
    if (-not ($edit | Where-Object { $_ -match ' watch paths=' })) { throw 'an edit never reached the watcher' }

    # Hidden behind another tab, edits start nothing.
    $tabs = (Invoke-Herdr tab list --workspace $ws.Workspace) | ConvertFrom-Json
    $reviewTab = @($tabs.result.tabs)[0].tab_id
    Invoke-Herdr tab create --workspace $ws.Workspace --focus | Out-Null
    Start-Sleep -Seconds 1
    $hidden = Get-LogWindow $log {
        foreach ($i in 1..10) {
            Add-Content -LiteralPath (Join-Path $repo $marker) -Value "hidden $i"
            Start-Sleep -Milliseconds 300
        }
    }
    $starts = @($hidden | Where-Object { $_ -match ' spawn ' })
    Write-Host "hidden 3s of edits: $($starts.Count) process starts"
    if ($starts.Count -ne 0) { throw "a hidden pane started processes: $($starts -join '; ')" }
    if (-not (Get-Content -LiteralPath $log | Where-Object { $_ -match 'visible=false' })) {
        throw 'the pane never saw itself hidden'
    }
    Invoke-Herdr tab focus $reviewTab | Out-Null

    $close = Invoke-PluginAction toggle -Plugin $plugin
    Write-Host "toggle: exit $($close.exit_code): $($close.stdout)$($close.stderr)"
    if ($close.exit_code -ne 0 -or $close.stdout -notmatch "^reviewr: closed $pane ") {
        throw "the second toggle did not close $pane"
    }
    Wait-For "pane $pane to close" {
        -not (Get-PaneIds $ws.Workspace | Where-Object { $_ -eq $pane })
    } | Out-Null
    Write-Host "toggle closed $pane"
} catch {
    # The actions run detached from the invoke, so their own output lives in herdr's log.
    Write-Host '--- herdr plugin log ---'
    try { Invoke-Herdr plugin log list --plugin $plugin | Write-Host } catch { Write-Host $_ }
    throw
} finally {
    try { Invoke-Herdr plugin unlink $plugin | Out-Null } catch {}
    Stop-HerdrServer
}
