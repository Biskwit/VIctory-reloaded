# Starts the title with the settings this fork is played with (README.md, "Getting started").
#   scripts\play.ps1 -Exe <path of your own copy's executable>
# -Safe         without the newer, faster ways (the first thing to try when something is wrong)
# -RayTracing   the title's own ray-traced indirect light: lit interiors, about a third slower
# -Windowed     a window instead of the whole screen (full screen is the default)
# -NoPanel      without the performance panel (on by default, F3 hides it while playing)
# -SkipShaders  start even when the shader cache is cold (otherwise every shader is translated first)
# -WarmPipelines  before launch, warm pipelines used in previous runs
# -Console      series-x (the default) or series-s: the console the title takes this PC for
# -FrameGen     AMD frame generation
# -Fsr / -Dlss  PC upscaling in Series S (quality, balanced or performance)
# -Fsr4K        PC FidelityFX at 3840x2160 (quality, balanced or performance)
# -Dlss4K       NVIDIA DLSS at 3840x2160 (quality, balanced or performance); FSR if it cannot start
# -FrameIntervalMs  use a frame interval in milliseconds (0 removes the limit)
# -Build        target\diag (the default, the build all measurements are of) or target\release
# -Machine      auto (the default: by GPU vendor; on NVIDIA, worker counts sized to the CPU and,
#               for a card of 10 to 19 GB, title buffers in the card's memory),
#               7900xtx, 4070, 4070tisuper, 4090, 3060 or laptop
#               (3060: a six-core NVIDIA desktop with 16 GB of RAM and a 12 GB card, docs/rtx3060-16gb.md;
#               laptop: an NVIDIA PC with 16 GB of RAM and a 6-8 GB card, docs/laptop-16gb.md)
# -More         other settings: -More @{VICTORY_PANEL='1'} or -More @{VICTORY_AMBIENT='1.0'}
# On a smaller NVIDIA PC, compare the same Series S scene after a fresh start
# with one override at a time, e.g. -More @{VICTORY_LAND_THREADS='3'} for
# copy-back work, or -More @{VICTORY_THIN_THREADS='6'} for list translation.
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$Safe,
    [switch]$RayTracing,
    [switch]$Windowed,
    [switch]$NoPanel,
    [switch]$SkipShaders,
    [switch]$WarmPipelines,
    [ValidateSet('series-x', 'series-s')][string]$Console = 'series-x',
    [switch]$FrameGen,
    [ValidateSet('quality', 'balanced', 'performance')][string]$Fsr,
    [ValidateSet('quality', 'balanced', 'performance')][string]$Dlss,
    [ValidateSet('quality', 'balanced', 'performance')][string]$Fsr4K,
    [ValidateSet('quality', 'balanced', 'performance')][string]$Dlss4K,
    [ValidateRange(0.0, 1000.0)][double]$FrameIntervalMs,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [ValidateSet('auto', '7900xtx', '4070', '4070tisuper', '4090', '3060', 'laptop')][string]$Machine = 'auto',
    [hashtable]$More = @{}
)
$settings = @{
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_TITLE_PRIORITIES = '0'
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0.8'; VICTORY_D3D12_DRED = 'faults'
}
$ram = (Get-CimInstance Win32_ComputerSystem -ErrorAction SilentlyContinue).TotalPhysicalMemory
$controllers = @(if ($Machine -in 'auto', '7900xtx') {
    Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue
})
$nvidiaControllers = @($controllers | Where-Object { $_.PNPDeviceID -match 'VEN_10DE' })
$hasNvidia = $nvidiaControllers.Count -gt 0
$autoNvidia = $Machine -eq 'auto' -and $hasNvidia
$cardGb = 0
$autoAdds = @{}
if ($Machine -eq 'auto') {
    # Keep the vendor defaults for rendering. The measured machine profiles are explicit.
    $Machine = if ($hasNvidia) { '4090' } else { '7900xtx' }
    # Read the driver key for the detected NVIDIA adapter. Old driver entries and
    # ambiguous adapter selections must not determine the automatic memory profile.
    if ($autoNvidia -and -not $Safe -and $nvidiaControllers.Count -eq 1 -and
        -not $More.ContainsKey('VICTORY_D3D12_ADAPTER') -and
        (Get-Command Get-PnpDeviceProperty -ErrorAction SilentlyContinue)) {
        $driverKey = (Get-PnpDeviceProperty -InstanceId $nvidiaControllers[0].PNPDeviceID -KeyName 'DEVPKEY_Device_Driver' -ErrorAction SilentlyContinue).Data
        if ($driverKey -match '^\{4d36e968-e325-11ce-bfc1-08002be10318\}\\\d+$') {
            $entry = Get-ItemProperty -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Control\Class\$driverKey" -Name 'HardwareInformation.qwMemorySize' -ErrorAction SilentlyContinue
            $cardBytes = $entry.'HardwareInformation.qwMemorySize' -as [uint64]
            $cardGb = [int][math]::Round($cardBytes / 1GB)
        }
    }
}

# What -Machine auto adds to the NVIDIA defaults for a card of $CardGb GB and a CPU of $Cores cores.
# - 10 to 19 GB: the memory settings of -Machine 4070 (and, from 15 GB, of -Machine 4070tisuper).
#   Under a 20 GiB budget the defaults leave title buffers under 4 MiB in the PC's memory, where
#   every access a shader makes crosses the bus: 16 fps against 29 on an RTX 5080
#   (docs/rtx5080-16gb.md), 7.4 against 14.7 on an RTX 3080 10 GB (docs/rtx4070-performance.md).
#   Not under -Safe, as in those presets. Smaller cards keep the defaults: the laptop preset
#   that fits them trades other things (docs/laptop-16gb.md).
# - Up to eight cores: those presets' three pipeline workers. The default ten, next to the
#   page-mapping threads, left 125 pipelines unmade at the start of the world on eight cores.
function Get-AutoNvidiaSettings([int]$CardGb, [int]$Cores, [bool]$Safe) {
    $adds = [ordered]@{}
    if ($Cores -le 8) { $adds.VICTORY_PIPELINE_WORKERS = '3' }
    if (-not $Safe -and $CardGb -ge 10 -and $CardGb -lt 20) {
        $adds.VICTORY_CARD_RESERVE_BUDGETS = '1'
        $adds.VICTORY_CARD_BUFFERS_MIN_KB = '64'; $adds.VICTORY_CARD_BUFFERS_MB = '1536'
        $adds.VICTORY_CARD_WINDOWS_MB = '1024'
        if ($CardGb -ge 15) { $adds.VICTORY_WINDOWS_BAR_MB = '4096' }
    }
    $adds
}
if ($Machine -eq '7900xtx') {
    # i9-12900K, RX 7900 XTX, 64 GB: the page-mapping lock was fastest with one
    # thread, and the AMD driver lowers its clocks between submissions.
    $settings.VICTORY_MAP_THREADS = '1'; $settings.VICTORY_CARD_AWAKE = '1'

    # Issue #138: these driver and adapter combinations can reset at world load.
    if (-not $hasNvidia -and -not $More.ContainsKey('VICTORY_RELAY')) {
        $os = Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue
        if (($os.BuildNumber -as [int]) -ge 22000) {
            $ltsc = $os.Caption -match '\bLTSC\b'
            $affected = $controllers | Where-Object {
                $_.PNPDeviceID -match 'VEN_1002' -and (
                    ($ltsc -and $_.Name -match '\bRX\s+6700\s+XT\b' -and
                        $_.DriverVersion -in '32.0.21045.26001', '32.0.32029.2010') -or
                    ($_.Name -match '\bRX\s+6900\s+XT\b' -and $_.DriverVersion -eq '32.0.21045.29003')
                )
            } | Select-Object -First 1
            if ($affected) {
                $settings.VICTORY_RELAY = '0'
                Write-Host "Using relay mode 0 for $($affected.Name) on $($os.Caption) (driver $($affected.DriverVersion), issue #138)."
            }
        }
    }
} else {
    # RTX 4090, Ryzen 9 9950X3D (16 cores of one kind, two caches), 48 GB.
    # NVIDIA sets its clocks by the driver's power mode: no spinner on the compute queue.
    $record, $update, $land = 12, 8, 6
    if ($autoNvidia -or $Machine -eq 'laptop') {
        $coreCount = (Get-CimInstance Win32_Processor -ErrorAction SilentlyContinue |
            Measure-Object -Property NumberOfCores -Sum).Sum
        if (-not $coreCount) {
            # The physical topology is unknown. Use the smaller desktop defaults.
            $coreCount = [math]::Min(8, [Environment]::ProcessorCount)
        }
        $coreCount = [math]::Min($coreCount, [Environment]::ProcessorCount)
        if ($coreCount -le 6) {
            $record, $update, $land = $coreCount, [math]::Min(4, $coreCount), [math]::Min(3, $coreCount)
        } elseif ($coreCount -lt 16) {
            $record, $update, $land = [math]::Min(8, $coreCount), 6, 4
        }
    }
    if ([Environment]::ProcessorCount -lt 32 -and $record -gt 8) {
        # Fewer logical processors than the 9950X3D's 32: an i9-12900K has 24
        # (8 fast cores of two threads, 8 slow ones), and 12/8/6 ran more of
        # our threads beside the submitting one than its cores have. Series S
        # cell, RTX 4090 with an i9-12900K (Vice-Tay 22c6eac merged in): 8/6/3
        # took the frame 58.0-60.4 -> 54.3-56.1 ms (the submitting thread's
        # own work 31 -> 27 ms of CPU); 6/4/3 with VICTORY_MAP_THREADS=1 gained
        # nothing (59.2). With the card buffers below and write-watched XMem
        # blocks, on 4fa2f0b: 57.3-57.6 -> 49.4-51.5 ms (README, our PC).
        # (Its 16 cores take 12/8/6 by the sizing above.)
        $record, $update, $land = 8, 6, 3
    }
    $settings.VICTORY_THIN_THREADS = [string]$record
    $settings.VICTORY_UNDER_THREADS = [string]$update
    $settings.VICTORY_LAND_THREADS = [string]$land
    # A fifth of the ambient estimate, not 0.8: in the prologue's cell on this
    # card 0.8 lit the wall and the dark right side 2-3 times as bright as the
    # console-like picture of our earlier tree; 0.2 is the least tried without
    # the purple probe blocks that no stand-in leaves, at the same frame time.
    $settings.VICTORY_AMBIENT = '0.2'
    # Title buffers from 64 KiB up, not 1 MiB, get a copy in the card's own
    # memory: in the PC's memory every load a shader makes of them crosses
    # the bus. A replay of the Series S cell: 34.7 -> 24.3 ms on the card;
    # live on an i9-12900K the card was busy 46 -> 28 ms a frame and the
    # frame went 58.0 -> 52.5 ms (Vice-Tay 22c6eac merged in); on 4fa2f0b
    # with the threads above and write-watched XMem blocks the card is busy
    # 27-35 ms of a 49-52 ms frame. The card's memory in use grew 0.4 GB.
    $settings.VICTORY_CARD_BUFFERS_MIN_KB = '64'
    if (-not $Safe) {
        # Pages are mapped by several threads only while the title streams
        # (not the default: one run behind it).
        $settings.VICTORY_MAP_THREADS = 'auto'
        # A fill of the constant-buffer ring that a small copy at its place writes
        # into refused the whole submission to the card steps; in list order it is
        # taken (submit thread 104.7 -> 50.9 ms a frame, 12.1 -> 14.3 fps here).
        $settings.VICTORY_PLACED_ORDER = '1'
        # Windows of title memory (what the title's own allocator hands its
        # draws) lie in the card's memory, written by the CPU through the bus
        # (Vice-Tay bd68f5e's switch): in the PC's memory every read a shader
        # makes of them crosses it. Sized for this card's 24 GB: the cell's
        # windows are about 5 GB, and the card's use went 7.6 -> 12.7 GB of
        # 23.4; at most 6 GB go there, so that textures keep their room in
        # bigger scenes (the windows past it stay in the PC's memory). A card
        # with a budget under 20 GiB, or without resizable BAR, keeps them all
        # in the PC's memory. Series S cell, i9-12900K: card busy 27.2-32.5 ->
        # 21.4 ms a frame, the submitting thread as it was.
        $settings.VICTORY_WINDOWS_BAR_MB = '6144'
        # The title's writes into guarded view memory go in beside the guard
        # (victory-gpu titlewrite.rs): its memcpy, memmove and memset look at
        # the guards first, and a store that faults is done through our own
        # view of the page, or left out if it changes nothing. Series S cell,
        # i9-12900K, two launches of an in-run A/B: write faults that open a
        # block 298-307 -> 199-205 a frame, waits for the guards' lock 54-60 ->
        # 32-36 ms a frame of all threads (most of them the title's), the
        # frame as it was.
        $settings.VICTORY_STORE_BESIDE = '1'; $settings.VICTORY_COPY_BESIDE = '1'
        # A fill behind a copy at its place reads the copy's source on the card. Without
        # it, a submission whose CPU-side fill depends on such a copy waits for the
        # flights: in play on an RTX 5080, 0.7 submissions a frame refused and waited
        # against 0.13, and frames 1.7 ms shorter (docs/rtx5080-16gb.md).
        $settings.VICTORY_PLACED_FILL_REDIRECT = '1'
    }
    if ($autoNvidia -or $Machine -eq 'laptop') {
        $autoAdds = Get-AutoNvidiaSettings $cardGb $coreCount ([bool]$Safe)
        foreach ($k in $autoAdds.Keys) { $settings[$k] = $autoAdds[$k] }
    }
}
if ($Machine -in '4070', '3060') {
    # Ryzen 7 7800X3D: eight physical cores, versus the 4090 test PC's sixteen.
    # Keep the same rendering profile; spend VRAM on
    # shader buffers before textures consume it. No approximate shader modes.
    $settings.VICTORY_THIN_THREADS = '8'; $settings.VICTORY_UNDER_THREADS = '6'; $settings.VICTORY_LAND_THREADS = '4'
    $settings.VICTORY_MAP_THREADS = '1'; $settings.VICTORY_PIPELINE_WORKERS = '3'
    if (-not $Safe) {
        $settings.VICTORY_CARD_RESERVE_BUDGETS = '1'
        $settings.VICTORY_CARD_BUFFERS_MIN_KB = '64'; $settings.VICTORY_CARD_BUFFERS_MB = '1536'
        $settings.VICTORY_CARD_WINDOWS_MB = '1024'
    }
}
if ($Machine -eq '3060') {
    # i5-12400F (six cores, twelve threads), RTX 3060 12 GB, 16 GB (docs/rtx3060-16gb.md); asked for
    # explicitly, the measurements cover that one machine. The 4070 settings above, with fewer
    # threads for six cores, pages mapped by several threads while the title streams, and up to
    # 3 GB of windows in the card's memory (Resizable BAR) instead of the RAM 16 GB doesn't have.
    $settings.VICTORY_THIN_THREADS = '6'; $settings.VICTORY_UNDER_THREADS = '4'; $settings.VICTORY_LAND_THREADS = '3'
    $settings.VICTORY_WINDOWS_BAR_MB = '3072'
    if (-not $Safe) { $settings.VICTORY_MAP_THREADS = 'auto' }
}
if ($Machine -eq '4070tisuper') {
    # RTX 4070 Ti SUPER (16 GB, DXGI budget about 15.3 GB), Ryzen 7 9850X3D (eight cores), 32 GB.
    # The 4070 profile's threads and card budgets: doubling them (3 GB of buffer copies, 4 GB of
    # windows) took the card from 6.8 to 9.3 GB in the prison walk with the same frames (median
    # 30, p10 27.5) and 2.5 GB more private memory, since a card copy keeps its CPU side in RAM.
    # What the 16 GB card can take instead is the windows themselves, with no CPU side:
    # up to 4 GB of them in card memory the CPU maps. That needs Resizable BAR (the GPU upload
    # heap); without it the setting does nothing and windows stay as on the 4070 profile.
    $settings.VICTORY_THIN_THREADS = '8'; $settings.VICTORY_UNDER_THREADS = '6'; $settings.VICTORY_LAND_THREADS = '4'
    $settings.VICTORY_MAP_THREADS = '1'; $settings.VICTORY_PIPELINE_WORKERS = '3'
    if (-not $Safe) {
        $settings.VICTORY_CARD_RESERVE_BUDGETS = '1'
        $settings.VICTORY_CARD_BUFFERS_MIN_KB = '64'; $settings.VICTORY_CARD_BUFFERS_MB = '1536'
        $settings.VICTORY_CARD_WINDOWS_MB = '1024'; $settings.VICTORY_WINDOWS_BAR_MB = '4096'
    }
}
if ($Machine -eq 'laptop') {
    # i7-13700H, RTX 3050 6 GB Laptop GPU, 16 GB (docs/laptop-16gb.md); asked for explicitly,
    # the measurements cover that one machine. On top of the NVIDIA settings above,
    # what makes the world load fit in 16 GB. These stay under -Safe too,
    # since without them the world load runs out of RAM.
    # Game files read past Windows' file cache, title buffers in its private memory used in
    # place, windows outside buffers cut on 64 KiB with their tails, and up to 3 GB of them in
    # the card's memory (Resizable BAR) instead of RAM. The card's own copies of title buffers
    # get 256 MB, which leaves the card room for those windows.
    $settings.VICTORY_DSTORAGE_IO = 'direct'; $settings.VICTORY_INPLACE = '2'
    $settings.VICTORY_WINDOW_KB = '64'; $settings.VICTORY_WINDOW_TAILS = '1'
    $settings.VICTORY_WINDOWS_BAR_MB = '3072'; $settings.VICTORY_CARD_BUFFERS_MB = '256'
    $settings.VICTORY_LEVELS_ON_DEMAND = '1'
    # A ceiling on the game's working set: past it Windows trims the game's own pages, not the
    # rest of the PC's (16 GB: 4,500 MB, which leaves about 11.6 GB for Windows, the memory the
    # card shares and the desktop; with no ceiling free RAM ran out before the world loaded).
    if ($ram) {
        $settings.VICTORY_WORKING_SET_MB = [string][math]::Max(3072, [int64]($ram / 1MB) - 11624)
    }
    # A 1080p output drawn at half size by the title's own upscaler: the card stays at about
    # 4.2 GB of a 5.2 GB budget.
    $settings.VICTORY_OUTPUT = '1920x1080'; $settings.VICTORY_UPSCALER = '6'; $settings.VICTORY_RENDER_SCALE = '0.5'
    # No RAM throttle and no change to the title's watchdog here: the world load fits with the
    # settings above (docs/laptop-16gb.md); for longer sessions see #96 and #126 there.
}
# -Safe leaves out every way still on trial, and the newest faster ones.
if ($Safe) {
    $settings.VICTORY_UNDER_WIDE = '0'; $settings.VICTORY_LAND_ECHO = '0'
} else {
    $settings.VICTORY_THIN = '1'; $settings.VICTORY_JOIN_PARTS = '8'; $settings.VICTORY_UPLOAD_AHEAD = '1'
    # A dispatch does not end a batch of argument clamps before the title waits
    # for it: off by default, as other ways the title may wait are not known.
    $settings.VICTORY_INDIRECT_BATCH = '2'
    # What FSR4's passes (fsr4_model_v07_fp8_no_scale_*, these 20 programs of
    # this version of the title) write to their scratch, one 21 MB block whole
    # every frame, stays on the card: the title's CPU never touches it (no
    # access in a Series S cell launch with VICTORY_LAND_NOT_PROBE=1), and it
    # was a third of what the relay fetched and merged into title memory.
    $settings.VICTORY_LAND_NOT = 'f46117391dd81461,8e614708143ef917,3d147e8190574516,528278df1274cb33,f7fc6c984427fef3,' +
        'c99e30956be0deaf,8cbd7f5b9ce7530c,85dea520c893a3d9,32aa8384586e0f12,f5ab5dcf95536794,47d8c225abfcb246,' +
        '8fb5e4aa9c542c3f,55c46e739fa0796a,17af85f170e66f30,80a0178d3a9d8d12,477b52e73ddeb6f2,8b437ab61d8008fe,' +
        'fdce2a0a9fb1521a,6a6c7b7fde59134f,ba578b59c8c28a57'
}
if ($RayTracing) { $settings.VICTORY_BVH_BUILDER = '1' }
# The whole screen, with no frame, as on the console.
if (-not $Windowed) { $settings.VICTORY_FULLSCREEN = '1' }
# The performance panel (FPS, frame times, GPU, CPU, VRAM, RAM) in the upper left
# corner, so clips and screenshots show what the build runs at and on what PC.
if (-not $NoPanel) { $settings.VICTORY_PANEL = '1' }
if ($FrameGen) { $settings.VICTORY_FRAMEGEN = '1' }
if ($Fsr -or $Dlss) {
    $settings.Remove('VICTORY_UPSCALER')
    $settings.Remove('VICTORY_RENDER_SCALE')
}
if ($Fsr) { $settings.VICTORY_FSR = $Fsr.ToLowerInvariant() }
if ($Dlss) { $settings.VICTORY_DLSS = $Dlss.ToLowerInvariant() }
if ($Fsr4K) { $settings.VICTORY_FSR_4K = $Fsr4K.ToLowerInvariant() }
if ($Dlss4K) { $settings.VICTORY_DLSS_4K = $Dlss4K.ToLowerInvariant() }
if ($PSBoundParameters.ContainsKey('FrameIntervalMs')) {
    $settings.VICTORY_FRAME_INTERVAL_MS = $FrameIntervalMs.ToString([Globalization.CultureInfo]::InvariantCulture)
}
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }

$profile = if ($autoNvidia) { 'automatic NVIDIA profile' } else { $Machine }
Write-Host "Settings for the $profile$(if ($Safe) { ', safe' })$(if ($RayTracing) { ', with ray tracing' })."
if ($autoAdds.Contains('VICTORY_CARD_BUFFERS_MIN_KB')) {
    $preset = if ($cardGb -ge 15) { '4070tisuper' } else { '4070' }
    Write-Host ("This NVIDIA card has $cardGb GB: title buffers go to the card's memory, with the memory settings of " +
        "-Machine $preset (docs/rtx5080-16gb.md). -Machine 4090 keeps the 4090's card budgets.")
}
if ($Console -eq 'series-s' -and $Machine -ne '7900xtx') {
    Write-Host ("Series S requests {0} list translation threads, {1} for memory updates, and {2} for landing." -f
        $settings.VICTORY_THIN_THREADS, $settings.VICTORY_UNDER_THREADS, $settings.VICTORY_LAND_THREADS)
}
if ($ram -and $ram -lt 60GB) {
    Write-Host ("Windows reports {0:N1} GiB of physical RAM. Keep a system-managed page file. See docs/measurements.md for measured Series S memory use." -f ($ram / 1GB))
}

$victory = Join-Path $PSScriptRoot "..\target\$Build\victory.exe"
if (-not (Test-Path -LiteralPath $victory -PathType Leaf)) { Write-Host "No $victory. Build it first (README.md)."; exit 1 }
if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { Write-Host "No $Exe."; exit 1 }
$victory = (Resolve-Path -LiteralPath $victory).Path
Write-Host "VIctory executable $victory"
try {
    $checksum = Get-FileHash -LiteralPath $victory -Algorithm SHA256 -ErrorAction Stop
    Write-Host "VIctory SHA-256 $($checksum.Hash)"
} catch {
    Write-Warning "Could not read the VIctory executable checksum: $($_.Exception.Message)"
}
# (Other settings left over in this window go; where your files are stays.)
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_FIDELITYFX_DIR', 'VICTORY_FFX_FG_DIR', 'VICTORY_DLSS_DIR', 'VICTORY_BCPACK_TABLES'
$inherited = @(Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $paths })
foreach ($entry in $inherited) { Remove-Item "Env:$($entry.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }
$discarded = $inherited | Where-Object {
    $_.Name -notin $More.Keys -and -not $_.Value.Equals([Environment]::GetEnvironmentVariable($_.Name))
} | Sort-Object Name
if ($discarded) {
    Write-Host "Discarded inherited values for $($discarded.Name -join ', '). Use -More to pass them explicitly."
}
if ($hasNvidia -or $Machine -ne '7900xtx') {
    $waveWideActive = $settings['VICTORY_WAVE_WIDE'] -eq '1' -and [string]::IsNullOrEmpty([string]$settings['VICTORY_WAVE_WIDE_ONLY'])
    $inheritedWaveWide = $inherited | Where-Object { $_.Name -eq 'VICTORY_WAVE_WIDE' } | Select-Object -First 1
    $inheritedWaveWideOnly = $inherited | Where-Object { $_.Name -eq 'VICTORY_WAVE_WIDE_ONLY' } | Select-Object -First 1
    $waveWideInherited = -not $More.ContainsKey('VICTORY_WAVE_WIDE') -and
        $inheritedWaveWide -and $inheritedWaveWide.Value -eq '1' -and
        (-not $inheritedWaveWideOnly -or [string]::IsNullOrEmpty($inheritedWaveWideOnly.Value))
    if ($waveWideActive) {
        Write-Warning "VICTORY_WAVE_WIDE=1 without VICTORY_WAVE_WIDE_ONLY enables unrestricted wave-wide barriers across all wave64 compute shaders on NVIDIA, which can cause TDR (0x887A0006) hangs. Since PR #161, the 4 safe shaders are enabled by default; unset VICTORY_WAVE_WIDE or pass VICTORY_WAVE_WIDE_ONLY."
    } elseif ($waveWideInherited) {
        Write-Warning "Inherited VICTORY_WAVE_WIDE=1 without VICTORY_WAVE_WIDE_ONLY was discarded for this launch. Since PR #161, the 4 safe shaders are enabled by default; VICTORY_WAVE_WIDE=1 alone enables unrestricted wave-wide barriers across all wave64 compute shaders on NVIDIA (which can cause TDR 0x887A0006 hangs). Unset VICTORY_WAVE_WIDE or pass VICTORY_WAVE_WIDE_ONLY."
    }
}
$Exe = (Resolve-Path -LiteralPath $Exe).Path
Push-Location -LiteralPath (Join-Path $PSScriptRoot '..')   # victory.toml is looked for here
try {
    if (-not $SkipShaders) {
        $config = & $victory config
        if ($LASTEXITCODE -eq 0) {
            $cache = $config | Select-String '^cache_dir\s*:\s*(.+)$' | Select-Object -First 1
            if ($cache) {
                $source = Split-Path -LiteralPath $Exe
                $game = $config | Select-String '^game_dir\s*:\s*(.+)$' | Select-Object -First 1
                if ($game -and (Test-Path -LiteralPath $game.Matches[0].Groups[1].Value -PathType Container)) {
                    $source = $game.Matches[0].Groups[1].Value
                }
                & $victory shaders --if-needed $source
                if ($LASTEXITCODE -ne 0) { Write-Host "victory shaders ended with exit code $LASTEXITCODE; starting anyway." }
            }
        }
    }
    if ($WarmPipelines) {
        & $victory warm --used
        if ($LASTEXITCODE -ne 0) { Write-Host "victory warm ended with exit code $LASTEXITCODE; starting anyway." }
    }
    & $victory run $Exe --console $Console --execute
    $runtimeExitCode = $LASTEXITCODE
} finally { Pop-Location }
exit $runtimeExitCode
