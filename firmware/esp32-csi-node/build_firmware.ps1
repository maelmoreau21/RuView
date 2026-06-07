param(
    [ValidateSet("esp32c6", "esp32s3")]
    [string]$Target = "esp32c6",

    [string]$Port = "",

    [string]$IdfPath = $env:IDF_PATH,

    [string]$IdfToolsPath = $env:IDF_TOOLS_PATH,

    [string]$IdfPythonEnvPath = $env:IDF_PYTHON_ENV_PATH,

    [switch]$Flash,

    [switch]$FullClean
)

$ErrorActionPreference = "Stop"

# ESP-IDF v5.4 rejects inherited MSYS/MinGW variables on Windows shells.
Remove-Item env:MSYSTEM -ErrorAction SilentlyContinue
Remove-Item env:MSYSTEM_CARCH -ErrorAction SilentlyContinue
Remove-Item env:MSYSTEM_CHOST -ErrorAction SilentlyContinue
Remove-Item env:MSYSTEM_PREFIX -ErrorAction SilentlyContinue
Remove-Item env:MINGW_CHOST -ErrorAction SilentlyContinue
Remove-Item env:MINGW_PACKAGE_PREFIX -ErrorAction SilentlyContinue
Remove-Item env:MINGW_PREFIX -ErrorAction SilentlyContinue

if ([string]::IsNullOrWhiteSpace($IdfPath)) {
    throw "Set IDF_PATH to an ESP-IDF v5.4 checkout, or pass -IdfPath C:\path\to\esp-idf."
}

$env:IDF_PATH = $IdfPath
if (-not [string]::IsNullOrWhiteSpace($IdfToolsPath)) {
    $env:IDF_TOOLS_PATH = $IdfToolsPath
}

$python = "python"
if (-not [string]::IsNullOrWhiteSpace($IdfPythonEnvPath)) {
    $candidate = Join-Path $IdfPythonEnvPath "Scripts\python.exe"
    if (Test-Path $candidate) {
        $python = $candidate
    }
}

$idf = Join-Path $env:IDF_PATH "tools\idf.py"
if (-not (Test-Path $idf)) {
    throw "idf.py not found at $idf"
}

$projectDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $projectDir

Write-Host "=== RuvSense ESP32 CSI firmware ==="
Write-Host "Target: $Target"
Write-Host "IDF:    $env:IDF_PATH"
Write-Host "Dir:    $projectDir"

& $python $idf set-target $Target
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if ($FullClean) {
    Write-Host "=== Full clean ==="
    & $python $idf fullclean
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & $python $idf set-target $Target
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

Write-Host "=== Building real WiFi CSI firmware (no mock mode) ==="
& $python $idf build
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if (Test-Path "sdkconfig") {
    $sdkconfig = Get-Content "sdkconfig"
    if ($sdkconfig -match "^CONFIG_CSI_MOCK_ENABLED=y") {
        throw "Refusing to continue: CONFIG_CSI_MOCK_ENABLED=y in sdkconfig."
    }
}

if ($Flash) {
    if ([string]::IsNullOrWhiteSpace($Port)) {
        throw "Pass -Port COMx when using -Flash."
    }
    Write-Host "=== Flashing $Target on $Port ==="
    & $python $idf -p $Port flash
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

Write-Host "=== Firmware build complete ==="
