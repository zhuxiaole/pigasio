# PigASIO 构建与打包
#
# 用法(在项目根目录执行):
#     powershell -ExecutionPolicy Bypass -File build.ps1
#
# 产物会收集到 dist\ 目录。三个文件必须放在一起才能工作 ——
# 驱动 DLL 要靠同目录的 pigasio-gui.exe 打开控制面板。

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

Write-Host ""
Write-Host "=== PigASIO 构建 ===" -ForegroundColor Cyan

# ---- 1. 检查工具链 ----
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    # 刚装完 Rust 但还没重开终端的常见情况:PATH 里还没有 cargo,
    # 但 rustup 已经把它装在默认位置了。直接去那儿找,省得用户白跑一趟。
    $fallback = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path (Join-Path $fallback 'cargo.exe')) {
        $env:Path = "$fallback;$env:Path"
        Write-Host "cargo 不在 PATH 里,临时使用 $fallback" -ForegroundColor Yellow
        Write-Host "(建议重开一个终端,让 PATH 生效)" -ForegroundColor DarkGray
    } else {
        Write-Host "找不到 cargo。" -ForegroundColor Red
        Write-Host "请先安装 Rust:https://rustup.rs"
        Write-Host "装完之后重新打开一个终端再运行本脚本。"
        exit 1
    }
}
Write-Host "工具链:$(cargo --version)"

# ---- 2. 编译 ----
Write-Host ""
Write-Host "正在编译(release,首次会比较慢)…" -ForegroundColor Cyan
& cargo build --release --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) {
    Write-Host "编译失败。" -ForegroundColor Red
    exit $LASTEXITCODE
}

# ---- 3. 收集产物 ----
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$files = @('pigasio_asio.dll', 'pigasio.exe', 'pigasio-gui.exe')
foreach ($f in $files) {
    $src = Join-Path $root "target\release\$f"
    if (-not (Test-Path $src)) {
        Write-Host "缺少产物:$f(编译应该没成功)" -ForegroundColor Red
        exit 1
    }

    $dst = Join-Path $dist $f
    # 先删掉旧文件再拷。Windows 不允许覆盖一个正被打开的文件(会报
    # "Device or resource busy"),但先删再建通常没问题 —— 这样用户
    # 开着控制面板也能重新打包。
    if (Test-Path $dst) {
        Remove-Item $dst -Force -ErrorAction SilentlyContinue
    }
    try {
        Copy-Item $src $dst -Force -ErrorAction Stop
    } catch {
        Write-Host ""
        Write-Host "无法写入 $dst" -ForegroundColor Red
        Write-Host ""

        # 查出到底是谁占着它。这一步很值得做 —— 被占用的几乎总是
        # "正在使用 PigASIO 的机架或 DAW",而用户往往想不到要去关它。
        $holders = @()
        try {
            $holders = & tasklist /m $f /fo csv /nh 2>$null |
                Where-Object { $_ -match '\S' } |
                ForEach-Object { ($_ -split '","')[0].Trim('"') } |
                Where-Object { $_ -and $_ -ne 'INFO:' } |
                Select-Object -Unique
        } catch { }

        if ($holders.Count -gt 0) {
            Write-Host "这个文件正被以下程序加载:" -ForegroundColor Yellow
            foreach ($h in $holders) { Write-Host "    $h" -ForegroundColor Yellow }
            Write-Host ""
            Write-Host "请关闭它们(通常是正在使用 PigASIO 的机架或 DAW),然后重新运行本脚本。" -ForegroundColor Yellow
            Write-Host "注意:只关掉 PigASIO 的控制面板是不够的 —— 机架进程本身也要关。" -ForegroundColor DarkGray
        } else {
            Write-Host "查不到具体是哪个进程占着它。可能是杀毒软件正在扫描,稍等一会儿再试。" -ForegroundColor Yellow
        }
        Write-Host ""
        Write-Host "原始错误:$($_.Exception.Message)" -ForegroundColor DarkGray
        exit 1
    }
}

# 把示例配置和说明一起带上,方便用户照着改。
$examples = Join-Path $root 'examples'
$examplesDst = Join-Path $dist 'examples'
if (Test-Path $examples) {
    if (Test-Path $examplesDst) {
        Remove-Item $examplesDst -Recurse -Force -ErrorAction SilentlyContinue
    }
    Copy-Item $examples $examplesDst -Recurse -Force
}

# ---- 4. 生成安装包 ----
#
# 安装包(installer\PigASIO.iss)会把驱动、控制面板和 CLI 一起装进去,并在
# 安装时注册 ASIO 驱动、卸载时反注册 —— 这是 dist\ 里那三个散装文件做不到
# 的,所以能出就出一份。
Write-Host ""
Write-Host "=== 生成安装包 ===" -ForegroundColor Cyan

# ISCC 从 PATH 里找(装 Inno Setup 时会问要不要加,选加就不用管)。
# 装在别处、或者装完没重开终端时,用环境变量 ISCC 指个明确路径 ——
# 路径不写死在这个脚本里,换台机器也能用。
$iscc = if ($env:ISCC) { $env:ISCC } else { 'iscc' }
if (-not (Get-Command $iscc -ErrorAction SilentlyContinue)) {
    Write-Host "没找到 ISCC,跳过生成安装包(不影响 dist\ 里的散装文件)。" -ForegroundColor Yellow
    Write-Host "装上 Inno Setup 后重开一个终端即可;若装在别处,可以这样指定:" -ForegroundColor DarkGray
    Write-Host "    `$env:ISCC = 'D:\software\Inno Setup 7\ISCC.exe'" -ForegroundColor DarkGray
} else {
    & $iscc (Join-Path $root 'installer\PigASIO.iss')
    if ($LASTEXITCODE -ne 0) {
        Write-Host "生成安装包失败。" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

Write-Host ""
Write-Host "=== 打包完成 ===" -ForegroundColor Green
Write-Host "输出目录:$dist"
Get-ChildItem $dist -File | Sort-Object Name | ForEach-Object {
    Write-Host ("  {0,-32} {1,12:N0} 字节" -f $_.Name, $_.Length)
}

Write-Host ""
Write-Host "接下来:" -ForegroundColor Cyan
Write-Host "  1) 先确认驱动能打开设备(不需要装驱动、不会出声):"
Write-Host "       cd `"$dist`""
Write-Host "       .\pigasio devices         列出所有音频设备"
Write-Host "       .\pigasio check           多设备同步自检"
Write-Host ""
Write-Host "  2) 用图形界面配置多设备:"
Write-Host "       .\pigasio-gui.exe"
Write-Host ""
Write-Host "  3) 注册到系统(需要**管理员身份**的终端),DAW 里才能看到:"
Write-Host "       .\pigasio install"
Write-Host ""
Write-Host "卸载:.\pigasio uninstall" -ForegroundColor DarkGray
Write-Host "提示:dist 目录整个拷到哪都行,但三个文件不能拆开。" -ForegroundColor DarkGray
Write-Host ""
Write-Host "分发给别人时用上面那个安装包:它会把三件套装到一起,并在安装时自动注册驱动、卸载时反注册。" -ForegroundColor DarkGray
