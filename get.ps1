# Fewer on Windows: the ready-made programs for this computer, then Fewer put into Claude Code.
#
#   irm https://github.com/hazemdawahi/fewer/releases/latest/download/install.ps1 | iex
#
# (in PowerShell). It downloads Fewer's two programs (fewer.exe and fewer-pack.exe) from the
# latest release, checks them against the release's SHA256SUMS, puts them in
# %USERPROFILE%\.local\bin (or FEWER_BIN_DIR), adds that folder to your PATH when it is not
# there yet, then runs `fewer install`, which shows every change it will make and asks once
# ($env:FEWER_YES = "1" first: answered for you). Nothing else is needed: no Rust, no git.
# Later: `fewer update` does this again; `fewer uninstall` takes it all out.
# (Published as the release's install.ps1; on Mac and Linux it is install.sh, from get.sh.)

& {
    $ErrorActionPreference = 'Stop'
    # (Invoke-WebRequest is many times faster without its progress bar)
    $ProgressPreference = 'SilentlyContinue'
    # (Windows PowerShell 5.1 on an older Windows may not offer TLS 1.2 by itself; GitHub needs it)
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'hazemdawahi/fewer'
    $base = if ($env:FEWER_DOWNLOAD) { $env:FEWER_DOWNLOAD } else { "https://github.com/$repo/releases/latest/download" }

    # (a 32-bit PowerShell on 64-bit Windows names the real system in PROCESSOR_ARCHITEW6432)
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { $target = 'x86_64-pc-windows-msvc' }
        # (Windows on Arm runs the x64 programs through its own emulation)
        'ARM64' { $target = 'x86_64-pc-windows-msvc' }
        default { throw "install: there is no ready-made Fewer for Windows on $arch yet. Build it from the source: https://github.com/$repo" }
    }

    $yourBin = $env:FEWER_BIN_DIR
    $bin = if ($yourBin) { $yourBin } else { Join-Path $HOME '.local\bin' }
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('fewer-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    $zip = "fewer-$target.zip"
    Write-Host "Downloading Fewer for Windows ($arch)..."
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$zip" -OutFile (Join-Path $tmp $zip)
    } catch {
        throw "install: could not download $base/$zip"
    }
    # (the release's checksums: the download is the one published)
    $sums = Join-Path $tmp 'SHA256SUMS'
    $haveSums = $true
    try { Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums } catch { $haveSums = $false }
    if ($haveSums) {
        $line = Get-Content $sums | Where-Object { $_ -match ('^[0-9a-f]{64}\s+\*?' + [regex]::Escape($zip) + '$') } | Select-Object -First 1
        $want = if ($line) { ($line -split '\s+')[0] } else { '' }
        $got = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $tmp $zip)).Hash.ToLower()
        if (-not $want -or $want -ne $got) {
            Remove-Item -Recurse -Force $tmp
            throw "install: the download does not match the release's checksum; nothing was installed"
        }
    }
    Expand-Archive -Path (Join-Path $tmp $zip) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    foreach ($name in 'fewer.exe', 'fewer-pack.exe') {
        $dest = Join-Path $bin $name
        # (a program that runs, as fewer's MCP server inside Claude Code, cannot be written over
        # on Windows, but it can be renamed: the new one takes its name, and the old one is
        # removed here once it no longer runs)
        Get-ChildItem -Path $bin -Filter "$name.old-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
        if (Test-Path $dest) {
            Rename-Item -Path $dest -NewName ("$name.old-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
        }
        Copy-Item -Path (Join-Path $tmp $name) -Destination $dest
        Get-ChildItem -Path $bin -Filter "$name.old-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force $tmp
    $fewer = Join-Path $bin 'fewer.exe'
    Write-Host "Installed $(& $fewer --version) and fewer-pack in $bin."

    # (the folder on your PATH: added to your user PATH when it is not on PATH yet; `fewer
    # uninstall` takes it out again; new terminals see it)
    $added = ''
    $key = 'HKCU:\Environment'
    $userPath = (Get-Item -Path $key).GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $same = { param($d) $d -and ($d.TrimEnd('\') -ieq $bin.TrimEnd('\')) }
    $onPath = @($env:Path -split ';') + @($userPath -split ';') | Where-Object { & $same $_ }
    if (-not $onPath) {
        $new = if ($userPath) { "$bin;$userPath" } else { $bin }
        Set-ItemProperty -Path $key -Name Path -Value $new -Type ExpandString
        # (Windows tells running programs of a change when one is made through .NET: a value
        # set and taken out again, so new terminals get the new PATH)
        [Environment]::SetEnvironmentVariable('FEWER_PATH_NOTICE', '1', 'User')
        [Environment]::SetEnvironmentVariable('FEWER_PATH_NOTICE', $null, 'User')
        $env:Path = "$bin;$env:Path"
        $added = $bin
        Write-Host "Added $bin to your PATH (new terminals see it)."
    }
    Write-Host ''

    # Claude Code's side: shown first, one question (or FEWER_YES=1)
    $env:FEWER_FROM = 'release'
    $env:FEWER_BIN_DIR = $bin
    $env:FEWER_PATH_USER = $added
    try {
        if ($env:FEWER_YES -eq '1') { & $fewer install --yes } else { & $fewer install }
    } finally {
        # (this PowerShell's own settings as they were)
        Remove-Item Env:FEWER_FROM, Env:FEWER_PATH_USER -ErrorAction SilentlyContinue
        $env:FEWER_BIN_DIR = $yourBin
    }
}
