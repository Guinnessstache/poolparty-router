# One-shot setup and self-test for the PoolParty router on jayspc.
#
#   powershell -ExecutionPolicy Bypass -File scripts\setup-and-test.ps1
#   (or double-click SETUP-AND-TEST.cmd)
#
# It finds llama-server and a model already on this PC, builds the router,
# starts both in the background, runs every routing and handoff check, prints
# a PASS/FAIL report and opens the dashboard. Until the Mac Studio arrives, the
# router's "mac" slot points at the same model on the 4070 Ti.
#
#   -Model <gguf>        use this model instead of the one it picks
#   -LlamaServer <exe>   use this llama-server instead of the one it finds
#   -NoTests             just set up and start everything
#   -Stop                stop everything this script started

param(
    [string]$Model,
    [string]$LlamaServer,
    [int]$RouterPort = 4000,
    [int]$ModelPort = 8081,
    [int]$Context = 16384,
    [switch]$NoTests,
    [switch]$Stop
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # makes Invoke-WebRequest much faster on PS 5.1

$onWindows = $env:OS -eq 'Windows_NT'
$root    = Split-Path $PSScriptRoot -Parent
$testDir = Join-Path $root 'test'
$pidFile = Join-Path $testDir 'running.pids'
$packaged = Join-Path $root 'PoolParty Router.exe'   # the downloaded app
$exe     = if (Test-Path $packaged) { $packaged } else { Join-Path $root ('target/release/poolparty-router' + $(if ($onWindows) { '.exe' } else { '' })) }
New-Item -ItemType Directory -Force -Path $testDir | Out-Null

function Say($msg, $color = 'Gray') { Write-Host $msg -ForegroundColor $color }
function Step($msg) { Write-Host ''; Write-Host "== $msg" -ForegroundColor Cyan }

# ---------------------------------------------------------------- stop
function Stop-Started {
    # Ask nicely first so the router also stops the model it started.
    foreach ($port in $RouterPort, ($RouterPort + 1)) {
        try { Invoke-WebRequest "http://127.0.0.1:$port/router/quit" -Method Post -Body '{}' -ContentType 'application/json' -UseBasicParsing -TimeoutSec 3 | Out-Null } catch {}
    }
    Start-Sleep -Milliseconds 800
    if (Test-Path $pidFile) {
        foreach ($id in Get-Content $pidFile) {
            if ($id -match '^\d+$') { Stop-Process -Id ([int]$id) -Force -ErrorAction SilentlyContinue }
        }
        Remove-Item $pidFile -Force
    }
}
if ($Stop) { Stop-Started; Say 'Stopped the test router and model.' Green; return }

# ---------------------------------------------------------------- http helpers
function Get-Ok($url) {
    try { (Invoke-WebRequest $url -UseBasicParsing -TimeoutSec 3).StatusCode -eq 200 } catch { $false }
}

function Send($path, $body, [hashtable]$headers = @{}, [int]$port = $RouterPort) {
    $json = $body | ConvertTo-Json -Depth 20 -Compress
    $bytes = [Text.Encoding]::UTF8.GetBytes($json)
    try {
        $r = Invoke-WebRequest "http://127.0.0.1:$port$path" -Method Post -Body $bytes `
               -ContentType 'application/json' -Headers $headers -UseBasicParsing -TimeoutSec 600
        $text = if ($r.Content -is [byte[]]) { [Text.Encoding]::UTF8.GetString($r.Content) } else { [string]$r.Content }
        return [pscustomobject]@{ Status = [int]$r.StatusCode; Node = "$($r.Headers['x-poolparty-node'])";
                                  Rule = "$($r.Headers['x-poolparty-rule'])"; Body = $text }
    } catch {
        $resp = $_.Exception.Response
        $code = if ($resp) { [int]$resp.StatusCode } else { 0 }
        return [pscustomobject]@{ Status = $code; Node = ''; Rule = ''; Body = $_.ErrorDetails.Message }
    }
}

function Start-Bg($file, [string[]]$argList, $logName) {
    $out = Join-Path $testDir "$logName.log"
    $err = Join-Path $testDir "$logName.err.log"
    $p = @{ FilePath = $file; ArgumentList = $argList; RedirectStandardOutput = $out;
            RedirectStandardError = $err; PassThru = $true }
    if ($onWindows) { $p.WindowStyle = 'Hidden' }
    $proc = Start-Process @p
    Add-Content $pidFile $proc.Id
    return $proc
}

function Wait-For($what, $url, $seconds) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $seconds) {
        if (Get-Ok $url) { Say "  $what is up ($([int]$sw.Elapsed.TotalSeconds)s)" Green; return $true }
        Start-Sleep -Seconds 2
    }
    return $false
}

# ---------------------------------------------------------------- 1. llama-server
Step 'Finding llama-server'
$home_ = if ($env:USERPROFILE) { $env:USERPROFILE } else { $HOME }
if (-not $LlamaServer) {
    $cmd = Get-Command llama-server -ErrorAction SilentlyContinue
    $candidates = @()
    if ($cmd) { $candidates += $cmd.Source }
    $candidates += @(
        (Join-Path $home_ '.unsloth\llama.cpp\build\bin\Release\llama-server.exe'),
        'C:\llama\llama-server.exe'
    )
    $LlamaServer = $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}
if (-not $LlamaServer) {
    throw 'No llama-server found. Get a Windows CUDA build from github.com/ggml-org/llama.cpp/releases, unzip it to C:\llama, and run this again (or pass -LlamaServer <path>).'
}
Say "  $LlamaServer"

# ---------------------------------------------------------------- 2. model
Step 'Choosing a model for the 4070 Ti'
if (-not $Model) {
    # Prefer models known to fit 12 GB with room for context, best first.
    $preferred = @(
        '.cache\huggingface\hub\Qwen3.8-27B-UD-IQ2_S.gguf',
        '.cache\huggingface\hub\Qwen3.8-27B-UD-Q2_K_XL.gguf',
        '.lmstudio\models\lmstudio-community\Qwen3.8-27B-UD-IQ3_XXS\Qwen3.8-27B-UD-IQ3_XXS.gguf'
    ) | ForEach-Object { Join-Path $home_ $_ }
    $Model = $preferred | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Model) {
    # Anything between 2 and 10 GB in the usual places, skipping vision projectors.
    $dirs = @('.lmstudio\models', '.cache\huggingface\hub', 'models', 'Downloads') |
            ForEach-Object { Join-Path $home_ $_ } | Where-Object { Test-Path $_ }
    $Model = Get-ChildItem $dirs -Recurse -Filter *.gguf -ErrorAction SilentlyContinue |
             Where-Object { $_.Name -notmatch 'mmproj' -and $_.Length -gt 2GB -and $_.Length -lt 10GB } |
             Sort-Object Length -Descending | Select-Object -First 1 -ExpandProperty FullName
}
if (-not $Model -or -not (Test-Path $Model)) {
    throw 'No suitable model found (2-10 GB GGUF). Pass one with -Model <path>.'
}
Say ("  {0}  ({1:N1} GB)" -f $Model, ((Get-Item $Model).Length / 1GB))

# ---------------------------------------------------------------- 3. build
Step 'Building the router'
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
$cargo = if ($cargo) { $cargo.Source } else { Join-Path $home_ '.cargo\bin\cargo.exe' }
$newest = Get-ChildItem (Join-Path $root 'src'), (Join-Path $root 'Cargo.toml') -Recurse -File -ErrorAction SilentlyContinue |
          Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($exe -eq $packaged) { Say '  Using the downloaded app' } elseif (-not (Test-Path $exe) -or $newest.LastWriteTime -gt (Get-Item $exe).LastWriteTime) {
    if (-not (Test-Path $cargo)) { throw 'Rust (cargo) not found. Install it from rustup.rs.' }
    Push-Location $root
    try { & $cargo build --release; if ($LASTEXITCODE -ne 0) { throw 'cargo build failed (see above).' } }
    finally { Pop-Location }
}
Say "  $exe" Green

# ---------------------------------------------------------------- 4. start
Step 'Starting the model and the router'
Stop-Started
foreach ($port in $RouterPort, $ModelPort) {
    if (Get-Ok "http://127.0.0.1:$port/health") {
        throw "Something is already running on port $port. Close it (or run with -Stop) and try again."
    }
}

$handoffs = Join-Path $testDir 'handoffs'
$cfgMain = Join-Path $testDir 'router.test.toml'
$cfgDown = Join-Path $testDir 'router.mac-down.toml'
$fwd = { param($p) $p -replace '\\', '/' }
# Main test router: runs the model itself; the remote ("Mac") slot points at the
# same model until the real Mac arrives.
@"
# Written by setup-and-test.ps1.
[router]
listen = ["127.0.0.1:$RouterPort"]
poll_seconds = 2
log_file = "router.log"

[nodes.local]
name = "This PC"

[nodes.remote]
name = "Mac Studio (stand-in)"
url = "http://127.0.0.1:$ModelPort/v1"

[local_model]
model = "$(& $fwd $Model)"
llama_server = "$(& $fwd $LlamaServer)"
context = $Context
port = $ModelPort
autostart = true

[escalation]
mode = "handoff"
strikes_to_escalate = 3
handoff_dir = "$(& $fwd $handoffs)"
"@ | Set-Content $cfgMain -Encoding ASCII

# Second router for the "Mac offline" checks: uses the running model as This PC
# without starting another copy, and points the Mac slot nowhere.
@"
[router]
listen = ["127.0.0.1:$($RouterPort + 1)"]
poll_seconds = 2
log_file = "router.mac-down.log"

[nodes.local]
url = "http://127.0.0.1:$ModelPort/v1"

[nodes.remote]
name = "Mac Studio (offline)"
url = "http://127.0.0.1:9/v1"

[local_model]
autostart = false

[escalation]
handoff_dir = "$(& $fwd $handoffs)"
"@ | Set-Content $cfgDown -Encoding ASCII

Start-Bg $exe @("`"$cfgMain`"", '--background') 'router' | Out-Null
if (-not (Wait-For 'Router' "http://127.0.0.1:$RouterPort/health" 30)) { throw 'Router failed to start; see test\router.log' }
Say '  Loading the model on the GPU (can take a minute)...'
$sw = [Diagnostics.Stopwatch]::StartNew(); $state = ''
while ($sw.Elapsed.TotalSeconds -lt 300) {
    $st = (Invoke-WebRequest "http://127.0.0.1:$RouterPort/status" -UseBasicParsing).Content | ConvertFrom-Json
    $state = $st.model.state
    if ($state -eq 'running' -and @($st.nodes | Where-Object { $_.online }).Count -eq 2) { break }
    if ($state -eq 'crashed') { throw "The model stopped: $($st.model.detail)" }
    Start-Sleep -Seconds 2
}
if ($state -ne 'running') { throw "The model didn't finish loading in 5 minutes (state: $state). See test\router.log" }
Say "  Model is ready ($([int]$sw.Elapsed.TotalSeconds)s)" Green

# ---------------------------------------------------------------- 5. tests
$results = New-Object System.Collections.ArrayList
function Check($name, [bool]$ok, $detail) {
    [void]$results.Add([pscustomobject]@{ Ok = $ok; Name = $name; Detail = $detail })
    if ($ok) { Say "  PASS  $name" Green } else { Say "  FAIL  $name  ->  $detail" Red }
}

if (-not $NoTests) {
    Step 'Running checks'
    $tools = @(@{ type = 'function'; function = @{ name = 'bash'; parameters = @{ type = 'object'; properties = @{ command = @{ type = 'string' } } } } })
    $noThink = @{ enable_thinking = $false }

    $st = (Invoke-WebRequest "http://127.0.0.1:$RouterPort/status" -UseBasicParsing).Content | ConvertFrom-Json
    $online = @($st.nodes | Where-Object { $_.online }).Count
    Check 'Router sees both machine slots online' ($online -eq 2) "$online of 2 online"

    $r = Send '/v1/completions' @{ model = 'auto'; prompt = 'def add(a, b):'; max_tokens = 16 }
    Check 'Tab completion goes to this PC' ($r.Status -eq 200 -and $r.Node -eq 'local') "status $($r.Status), went to '$($r.Node)'"

    $r = Send '/v1/chat/completions' @{ model = 'auto'; max_tokens = 128; chat_template_kwargs = $noThink
                                        messages = @(@{ role = 'user'; content = 'Reply with the single word: ready' }) }
    $answer = ''
    try { $j = $r.Body | ConvertFrom-Json; $answer = "$($j.choices[0].message.content)$($j.choices[0].message.reasoning_content)" } catch {}
    Check 'Chat goes to the Mac slot and the model answers' ($r.Status -eq 200 -and $r.Node -eq 'remote' -and $answer.Trim()) "status $($r.Status), node '$($r.Node)', answer '$($answer.Trim())'"

    $r = Send '/v1/chat/completions' @{ model = 'auto'; stream = $true; max_tokens = 16; chat_template_kwargs = $noThink
                                        messages = @(@{ role = 'user'; content = 'Count to three.' }) }
    Check 'Streaming passes through' ($r.Status -eq 200 -and $r.Body -match 'data: ') "status $($r.Status)"

    # A coding session that keeps hitting the same build error.
    $err = "Assets/Scripts/Knight.cs({0},9): error CS0103: The name 'jumpForce' does not exist in the current context`nBuild FAILED."
    function Stuck-Session($sessionId, $turns) {
        $msgs = New-Object System.Collections.ArrayList
        [void]$msgs.Add(@{ role = 'user'; content = "Make the knight jump when space is pressed ($sessionId)" })
        $last = $null
        for ($i = 0; $i -lt $turns; $i++) {
            [void]$msgs.Add(@{ role = 'assistant'; content = ''; tool_calls = @(@{ id = "b$i"; type = 'function';
                  function = @{ name = 'bash'; arguments = (@{ command = 'dotnet build'; attempt = $i } | ConvertTo-Json -Compress) } }) })
            [void]$msgs.Add(@{ role = 'tool'; tool_call_id = "b$i"; content = ($err -f (20 + $i)) })
            $last = Send '/v1/chat/completions' @{ model = 'auto'; max_tokens = 16; tools = $tools; chat_template_kwargs = $noThink
                                                   messages = $msgs.ToArray() } @{ 'x-session-id' = $sessionId }
            if ($last.Node -eq 'claude-code') { return @{ Turn = $i + 1; Resp = $last } }
        }
        return @{ Turn = 0; Resp = $last }
    }

    $s = Stuck-Session 'selftest-stuck' 5
    Check 'Repeated build error hands off to Claude Code on the 4th try' ($s.Turn -eq 4) "handed off on turn $($s.Turn)"
    $brief = Get-ChildItem $handoffs -Filter '*selftest-stuck*.md' -ErrorAction SilentlyContinue | Select-Object -First 1
    Check 'Handoff brief was written' ([bool]$brief) "nothing in $handoffs"
    if ($brief) {
        $md = Get-Content $brief.FullName -Raw
        Check 'Brief has the task, the error and a summary' ($md -match 'knight jump' -and $md -match 'CS0103' -and $md -match 'already tried') 'missing sections'
    }
    Check 'Agent is told the claude command' ($s.Resp.Body -match 'claude \\?"Read') 'no command in reply'

    $null = Invoke-WebRequest "http://127.0.0.1:$RouterPort/router/mode" -Method Post -Body '{"paused":true}' -ContentType 'application/json' -UseBasicParsing
    $s = Stuck-Session 'selftest-paused' 5
    Check 'Paused handoffs keep a stuck session local' ($s.Turn -eq 0 -and $s.Resp.Status -eq 200) "handed off on turn $($s.Turn)"
    $null = Invoke-WebRequest "http://127.0.0.1:$RouterPort/router/mode" -Method Post -Body '{"paused":false}' -ContentType 'application/json' -UseBasicParsing

    $null = Send '/v1/chat/completions' @{ model = 'auto'; max_tokens = 8; chat_template_kwargs = $noThink
                                           messages = @(@{ role = 'user'; content = 'hello (selftest-manual)' }) } @{ 'x-session-id' = 'selftest-manual' }
    $null = Invoke-WebRequest "http://127.0.0.1:$RouterPort/router/escalate" -Method Post -Body '{"session":"selftest-manual"}' -ContentType 'application/json' -UseBasicParsing
    $r = Send '/v1/chat/completions' @{ model = 'auto'; max_tokens = 8; messages = @(@{ role = 'user'; content = 'hello (selftest-manual)' }) } @{ 'x-session-id' = 'selftest-manual' }
    Check 'Dashboard "Hand off" button works' ($r.Node -eq 'claude-code') "went to '$($r.Node)'"

    # Mac offline: a second router whose Mac slot points nowhere.
    $down = Start-Bg $exe @("`"$cfgDown`"", '--background') 'router-mac-down'
    if (Wait-For 'Mac-offline router' "http://127.0.0.1:$($RouterPort + 1)/health" 30) {
        Start-Sleep -Seconds 3
        $r = Send '/v1/chat/completions' @{ model = 'auto'; max_tokens = 8; chat_template_kwargs = $noThink
                                            messages = @(@{ role = 'user'; content = 'hi' }) } @{} ($RouterPort + 1)
        Check 'Mac offline: chat falls back to this PC' ($r.Node -eq 'local') "went to '$($r.Node)' (status $($r.Status))"
        $r = Send '/v1/chat/completions' @{ model = 'auto'; max_tokens = 8; tools = $tools
                                            messages = @(@{ role = 'user'; content = 'build it' }) } @{} ($RouterPort + 1)
        Check 'Mac offline: agent turns are refused, not downgraded' ($r.Status -eq 503) "status $($r.Status)"
    } else { Check 'Mac-offline router started' $false 'see test\router-mac-down.err.log' }
    Stop-Process -Id $down.Id -Force -ErrorAction SilentlyContinue

    # Leave the test sessions out of the dashboard.
    foreach ($id in 'selftest-stuck', 'selftest-paused', 'selftest-manual') {
        $null = Invoke-WebRequest "http://127.0.0.1:$RouterPort/router/reset" -Method Post -Body "{`"session`":`"$id`"}" -ContentType 'application/json' -UseBasicParsing -ErrorAction SilentlyContinue
    }

    $passed = @($results | Where-Object { $_.Ok }).Count
    $report = Join-Path $testDir 'report.txt'
    @("PoolParty router self-test  $(Get-Date)", "Model: $Model", "llama-server: $LlamaServer", '') +
        ($results | ForEach-Object { "{0}  {1}{2}" -f $(if ($_.Ok) { 'PASS' } else { 'FAIL' }), $_.Name, $(if ($_.Ok) { '' } else { "  ->  $($_.Detail)" }) }) +
        @('', "$passed of $($results.Count) passed") | Set-Content $report
    Step "$passed of $($results.Count) checks passed   (saved to test\report.txt)"
}

# ---------------------------------------------------------------- 6. try it
$demo = Join-Path $testDir 'opencode-demo'
New-Item -ItemType Directory -Force -Path $demo | Out-Null
@{ '$schema' = 'https://opencode.ai/config.json'
   provider = @{ poolparty = @{ npm = '@ai-sdk/openai-compatible'; name = 'PoolParty'
                                options = @{ baseURL = "http://127.0.0.1:$RouterPort/v1" }
                                models = @{ auto = @{ name = 'PoolParty auto' } } } }
   model = 'poolparty/auto' } | ConvertTo-Json -Depth 10 | ForEach-Object {
    # No byte-order mark: Windows PowerShell's UTF8 adds one and JSON parsers choke on it.
    [IO.File]::WriteAllText((Join-Path $demo 'opencode.json'), $_, (New-Object Text.UTF8Encoding $false)) }

Write-Host ''
Say "Router:    http://127.0.0.1:$RouterPort/v1   (model auto)" White
Say "Dashboard: http://127.0.0.1:$RouterPort/" White
Say "OpenCode:  open the folder test\opencode-demo in OpenCode; it's already pointed at the router." White
Say "Stop it:   scripts\setup-and-test.ps1 -Stop   (or STOP.cmd)" White
if ($onWindows) { Start-Process "http://127.0.0.1:$RouterPort/" }
