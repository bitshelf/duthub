# duthub 发布门（本地 / CI 可复跑）
#
# goose 自 2026-11-25 起改为下载分发（ADR-3 §9），本脚本的断言随之改写：
#   1. goose 不得再内嵌：apps/desktop/assets/goose/ 不存在、bundle.rs 不编译资产、
#      dist/ 不携带 goose 可执行文件、不再旁附内嵌口径的 LICENSE/NOTICE/manifest；
#   2. 下载分发契约仍在：duthub-tooldown 的 TOOL_GOOSE spec（https URL + sha256 口径）、
#      安装内核的 pin 强制与 checksumVerified 记录、registry 的启动前复算；
#   3. 许可与来源说明仍在：根 NOTICE 的 goose 段、goose-distribution.md、ADR-3 §9；
#   4. duthub.exe 必须已通过 Authenticode 验证（未签名/证书无效即失败）；
#      非发布构建（-AllowUnsigned，main push 没有签名证书）降为 warning，并由
#      发布说明如实记录 Authenticode 状态；tag 发布不传该开关，仍 fail-closed；
#   5. 发布物旁附 dist/licenses/goose/PROVENANCE.md（与仓库源文件逐字节一致），
#      duthub 的 LICENSE/NOTICE 同样逐字节一致，旧口径的 goose 声明不得残留；
#   6. SBOM（CycloneDX 1.7）与 duthub 的 cargo-audit JSON 存在且无漏洞，且 cargo-audit
#      退出码为 0（workflow 把 $LASTEXITCODE 写入 audit-exit 文件；退出码非 0 说明审计
#      失败或发现漏洞，与 JSON 内容独立判定）；goose 运行时依赖审计 = 显式 UNVERIFIED
#      占位：duthub 不再分发 goose，无法审计其依赖树；workflow 必须写
#      dist/sbom/goose-audit-status.txt（以 UNVERIFIED 开头），本脚本断言该文件存在
#      （防止检查被静默删除）。若 owner 另行提供上游 Cargo.lock 的审计结果，则必须
#      干净且退出码 0；
#   7. 生成 dist/SHA256SUMS；
#   8. 生成 dist/RELEASE_NOTES.md。
#
# 失败一律 exit 1；脚本只打印文件路径、哈希与签名状态，不读取或打印任何 Token。
# 依赖社区工具：cargo-cyclonedx / cargo-audit / PowerShell Get-AuthenticodeSignature。

[CmdletBinding()]
param(
    [string]$RepoRoot,
    [string]$DistDir,
    [string]$EmbeddedGooseDir,
    [string]$SpecPath,
    [string]$ProvenancePath,
    [string]$SbomPath,
    [string]$AuditPath,
    [string]$AuditExitPath,
    [string]$GooseAuditPath,
    [string]$GooseAuditExitPath,
    [string]$GooseAuditStatusPath,
    [string]$ChecksumsPath,
    [string]$ReleaseNotesPath,
    # Non-release builds (main pushes) carry no code-signing certificate. The
    # flag downgrades Authenticode to a recorded warning; tag releases never
    # pass it, so a published exe must still be validly signed.
    [switch]$AllowUnsigned
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Stop-Gate([string]$Message) {
    Write-Host "RELEASE GATE FAIL: $Message" -ForegroundColor Red
    exit 1
}

function Resolve-GatePath([string]$Path, [string]$Base) {
    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $null
    }
    if ([System.IO.Path]::IsPathRooted($Path)) {
        return [System.IO.Path]::GetFullPath($Path)
    }
    return [System.IO.Path]::GetFullPath((Join-Path $Base $Path))
}

function Get-JsonValue($Object, [string]$Name) {
    if ($null -eq $Object) {
        return $null
    }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Assert-File([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Stop-Gate "$Label 缺失：$Path"
    }
}

function Assert-NonEmptyFile([string]$Path, [string]$Label) {
    Assert-File $Path $Label
    if ((Get-Item -LiteralPath $Path).Length -eq 0) {
        Stop-Gate "$Label 为空：$Path"
    }
}

# Case-insensitive containment check for documents whose exact wording may be
# reworded, but whose machine-checkable anchors (source URL, licence, policy
# statement) must stay. The label names the contract being defended.
function Assert-Contains([string]$Path, [string]$Text, [string]$Needle, [string]$Label) {
    if ($Text.IndexOf($Needle, [System.StringComparison]::OrdinalIgnoreCase) -lt 0) {
        Stop-Gate "$Label 缺少必需内容 '$Needle'：$Path"
    }
}

# Ordinal (case-sensitive) check for code tokens that must not reappear.
function Assert-NotContains([string]$Path, [string]$Text, [string]$Needle, [string]$Label) {
    if ($Text.IndexOf($Needle, [System.StringComparison]::Ordinal) -ge 0) {
        Stop-Gate "$Label 命中禁用内容 '$Needle'：$Path"
    }
}

function Assert-SameFile([string]$ExpectedPath, [string]$ActualPath, [string]$Label) {
    Assert-File $ExpectedPath "$Label 仓库源文件"
    Assert-File $ActualPath "$Label 发布物副本"
    $expected = (Get-FileHash -LiteralPath $ExpectedPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $ActualPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        Stop-Gate "$Label 发布物副本与仓库源文件不一致：$ActualPath"
    }
    Write-Host "staged ok: $Label ($actual)"
}

function Read-JsonFile([string]$Path, [string]$Label) {
    Assert-File $Path $Label
    try {
        return Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
    } catch {
        Stop-Gate "$Label 不是有效 JSON：$Path（$($_.Exception.Message)）"
    }
}

function Assert-AuditClean($Audit, [string]$Label) {
    $vulnerabilities = Get-JsonValue $Audit 'vulnerabilities'
    if ($null -eq $vulnerabilities) {
        Stop-Gate "$Label 结构无法识别：缺少 vulnerabilities 段（是否用了 cargo audit --json？）"
    }
    $found = Get-JsonValue $vulnerabilities 'found'
    $count = Get-JsonValue $vulnerabilities 'count'
    $hasVulnerabilities = $false
    if ($null -ne $found) {
        $hasVulnerabilities = [bool]$found
    } elseif ($null -ne $count) {
        $hasVulnerabilities = [int64]$count -gt 0
    } else {
        Stop-Gate "$Label 结构无法识别：vulnerabilities 段缺少 found/count"
    }
    if ($hasVulnerabilities) {
        Stop-Gate "$Label 报告了漏洞，拒绝发布"
    }
    Write-Host "audit ok: $Label"
}

function Assert-AuditExit([string]$ExitFilePath, [string]$AuditFilePath, [string]$Label) {
    Assert-File $ExitFilePath "$Label 退出码文件（workflow 以 `$LASTEXITCODE 写入）"
    $raw = (Get-Content -LiteralPath $ExitFilePath -Raw -Encoding UTF8).Trim()
    $exitCode = 0
    if (-not [int]::TryParse($raw, [ref]$exitCode)) {
        Stop-Gate "$Label 退出码文件不是整数：'$raw'（$ExitFilePath）"
    }
    if ($exitCode -ne 0) {
        Stop-Gate "$Label cargo-audit 退出码非 0（退出码 $exitCode）：审计运行失败或发现漏洞，拒绝发布。详见 $AuditFilePath"
    }
    Write-Host "audit exit ok: $Label (exit 0)"
}

if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    $RepoRoot = if ([string]::IsNullOrWhiteSpace($PSScriptRoot)) {
        (Get-Location).Path
    } else {
        Split-Path -Parent $PSScriptRoot
    }
}
$repo = [System.IO.Path]::GetFullPath($RepoRoot)
if (-not (Test-Path -LiteralPath $repo -PathType Container)) {
    Stop-Gate "仓库根目录不存在：$repo"
}

$dist = Resolve-GatePath $DistDir $repo
if ($null -eq $dist) { $dist = Join-Path $repo 'dist' }
# The directory that must NOT exist: the former embedded goose asset drop.
$embeddedGoose = Resolve-GatePath $EmbeddedGooseDir $repo
if ($null -eq $embeddedGoose) { $embeddedGoose = Join-Path $repo 'apps\desktop\assets\goose' }
$specFile = Resolve-GatePath $SpecPath $repo
if ($null -eq $specFile) { $specFile = Join-Path $repo 'crates\duthub-tooldown\src\spec.rs' }
$provenanceFile = Resolve-GatePath $ProvenancePath $repo
if ($null -eq $provenanceFile) { $provenanceFile = Join-Path $repo 'apps\desktop\assets\goose-distribution.md' }
$sbom = Resolve-GatePath $SbomPath $repo
if ($null -eq $sbom) { $sbom = Join-Path $dist 'sbom\duthub.cdx.json' }
$audit = Resolve-GatePath $AuditPath $repo
if ($null -eq $audit) { $audit = Join-Path $dist 'sbom\audit.json' }
$auditExit = Resolve-GatePath $AuditExitPath $repo
if ($null -eq $auditExit) { $auditExit = Join-Path $dist 'sbom\audit-exit.txt' }
$gooseAudit = Resolve-GatePath $GooseAuditPath $repo
if ($null -eq $gooseAudit) { $gooseAudit = Join-Path $dist 'sbom\audit-goose.json' }
$gooseAuditExit = Resolve-GatePath $GooseAuditExitPath $repo
if ($null -eq $gooseAuditExit) { $gooseAuditExit = Join-Path $dist 'sbom\audit-goose-exit.txt' }
$gooseAuditStatus = Resolve-GatePath $GooseAuditStatusPath $repo
if ($null -eq $gooseAuditStatus) { $gooseAuditStatus = Join-Path $dist 'sbom\goose-audit-status.txt' }
$checksums = Resolve-GatePath $ChecksumsPath $repo
if ($null -eq $checksums) { $checksums = Join-Path $dist 'SHA256SUMS' }
$notes = Resolve-GatePath $ReleaseNotesPath $repo
if ($null -eq $notes) { $notes = Join-Path $dist 'RELEASE_NOTES.md' }

Write-Host "release gate: repo=$repo dist=$dist forbidden-goose=$embeddedGoose spec=$specFile"

# --- 1. goose 不再内嵌：目录/源文件/发布物三处都不得出现 goose 字节 -----------
# 防的是：内嵌资产被悄悄加回仓库或构建——那会绕过「下载安装 + 安装 manifest 记录实测
# sha256 + 启动前复算」的链路，并让 ADR-3 §9 的许可/来源口径失效。Rust 侧有契约测试
# apps/desktop/tests/agent_host.rs::embedded_goose_assets_are_gone；这里是发布门的独立复验。
if (Test-Path -LiteralPath $embeddedGoose) {
    Stop-Gate "apps/desktop/assets/goose 仍存在：goose 已改为下载分发（ADR-3 §9），不得再随仓库/构建内嵌 goose 二进制或声明文件。若确需恢复内嵌，必须先改 ADR-3、补回上游 Cargo.lock 审计并重新签字。"
}
foreach ($name in @('goose-acp.exe', 'goose-x86_64-pc-windows-msvc.zip', 'goose.exe', 'Cargo.lock', 'manifest.json', 'LICENSE', 'NOTICE')) {
    $stale = Join-Path $embeddedGoose $name
    if (Test-Path -LiteralPath $stale) {
        Stop-Gate "发现内嵌 goose 残留：$stale（goose 只允许由用户从上游下载安装，ADR-3 §9）"
    }
}
$assetsRoot = Join-Path $repo 'apps\desktop\assets'
if (Test-Path -LiteralPath $assetsRoot -PathType Container) {
    $strayGooseExes = @(Get-ChildItem -LiteralPath $assetsRoot -Recurse -File -Filter 'goose*.exe')
    if ($strayGooseExes.Count -gt 0) {
        Stop-Gate "apps/desktop/assets 下仍有 goose 可执行文件：$($strayGooseExes[0].FullName)（ADR-3 §9 禁止内嵌）"
    }
}
foreach ($name in @('goose-acp.exe', 'goose.exe', 'goose-x86_64-pc-windows-msvc.zip')) {
    $staged = Join-Path $dist $name
    if (Test-Path -LiteralPath $staged) {
        Stop-Gate "dist/ 出现 goose 分发文件：$staged；发布物只含 duthub.exe，goose 由用户自行下载（ADR-3 §9）"
    }
}

# 防的是：bundle.rs 重新 include_bytes!/include_str! 一个资产（目录检查被绕过），
# 或应用内许可页重新宣称「已内嵌」而仓库实际没有内嵌。
$bundlePath = Join-Path $repo 'apps\desktop\src\agent_host\bundle.rs'
Assert-NonEmptyFile $bundlePath 'goose 兼容面 bundle.rs'
$bundleText = Get-Content -LiteralPath $bundlePath -Raw -Encoding UTF8
foreach ($forbidden in @('include_bytes!', 'include_str!', 'assets/goose')) {
    Assert-NotContains $bundlePath $bundleText $forbidden 'bundle.rs 不得再内嵌 goose 资产'
}
Assert-Contains $bundlePath $bundleText '"embedded": false' 'bundle.rs 分发记录'

$versionMatch = [regex]::Match($bundleText, 'pub const GOOSE_VERSION: &str = "(?<version>[0-9]+\.[0-9]+\.[0-9]+)";')
if (-not $versionMatch.Success) {
    Stop-Gate "无法从 bundle.rs 解析 GOOSE_VERSION（版本门槛必须保持单一来源）"
}
$gooseVersion = $versionMatch.Groups['version'].Value

# --- 2. duthub-tooldown 的 goose spec 与安装契约 -------------------------------
# 防的是：下载分发契约被删/被改坏——spec 消失 = 工具下载页装不到 goose；
# URL 换了未知主机 = 安装来源漂移；sha256 口径被抹掉 = 无人能说明下载的字节是什么。
Assert-NonEmptyFile $specFile 'duthub-tooldown spec.rs'
$specText = Get-Content -LiteralPath $specFile -Raw -Encoding UTF8
$specMatch = [regex]::Match($specText, '(?ms)(?<block>(?:///[^\r\n]*\r?\n)+pub const TOOL_GOOSE: ToolSpec = ToolSpec \{.*?\r?\n\};)')
if (-not $specMatch.Success) {
    Stop-Gate "crates/duthub-tooldown/src/spec.rs 缺少 TOOL_GOOSE 定义（goose 下载分发的唯一来源）"
}
$gooseSpec = $specMatch.Groups['block'].Value
if ($gooseSpec -notmatch 'id:\s*"goose"') {
    Stop-Gate "TOOL_GOOSE.id 必须是 goose"
}
$urlMatch = [regex]::Match($gooseSpec, 'url:\s*"(?<url>https://[^"]+)"')
if (-not $urlMatch.Success) {
    Stop-Gate "TOOL_GOOSE 缺少 https URL（下载分发必须有明确来源）"
}
$gooseUrl = $urlMatch.Groups['url'].Value
if ($gooseUrl -notmatch '^https://github\.com/aaif-goose/goose/releases/.+/goose-x86_64-pc-windows-msvc\.zip$') {
    Stop-Gate "TOOL_GOOSE URL 不是 AAIF/goose 官方 release 的 Windows x64 zip：$gooseUrl"
}
if ($specText -notmatch '(?ms)pub fn builtin_specs\(\).*?&TOOL_GOOSE') {
    Stop-Gate "TOOL_GOOSE 未登记进 builtin_specs()：spec 存在但工具下载页不会安装它"
}

# sha256 口径：pinned_sha256 必须显式写 Some("<64 hex>") 或 None。None（滚动 latest
# 资产不固定）是当前选择，但此时 spec 注释必须记录一次实测 sha256（64 位 hex）并说明
# 不固定 pin 的理由——否则「未验证下载」连可复核的基准都没有。
$pinMatch = [regex]::Match($gooseSpec, 'pinned_sha256:\s*(?<value>Some\("(?<hex>[0-9a-fA-F]{64})"\)|None)')
if (-not $pinMatch.Success) {
    Stop-Gate 'TOOL_GOOSE 必须显式声明 pinned_sha256: Some("<64 hex>") 或 None'
}
$digestMatch = [regex]::Matches($gooseSpec, '(?<![0-9a-fA-F])[0-9a-fA-F]{64}(?![0-9a-fA-F])')
if ($pinMatch.Groups['value'].Value -eq 'None') {
    if ($digestMatch.Count -eq 0) {
        Stop-Gate "TOOL_GOOSE 未固定 pin 时必须在 spec 注释记录实测 sha256（64 位 hex），否则无法复核下载内容"
    }
    if ($gooseSpec -notmatch '(?i)not pinned|unpinned|rolling') {
        Stop-Gate "TOOL_GOOSE 的 pinned_sha256: None 必须写明「滚动资产不固定 pin」的理由"
    }
} else {
    Write-Host "goose spec: pinned_sha256 = $($pinMatch.Groups['hex'].Value)"
}

# 防的是：安装内核丢掉 pin 强制或把未固定下载伪装成已验证。前者让未来重新 pin 时
# 不再拦截篡改，后者会让安装 manifest 谎称 checksumVerified。
$installPath = Join-Path $repo 'crates\duthub-tooldown\src\install.rs'
Assert-NonEmptyFile $installPath 'tooldown 安装内核 install.rs'
$installText = Get-Content -LiteralPath $installPath -Raw -Encoding UTF8
Assert-Contains $installPath $installText 'ChecksumMismatch' '安装内核必须在 pin 不符时硬失败'
Assert-Contains $installPath $installText 'if let Some(pin) = spec.pinned_sha256' '安装内核必须执行 spec 的 pin'
Assert-Contains $installPath $installText 'checksum_verified: spec.pinned_sha256.is_some()' '安装 manifest 必须把未固定下载记为未验证'

# 防的是：桌面启动链不再复算已下载 bytes，或「未安装」不再是可展示状态。
$registryPath = Join-Path $repo 'apps\desktop\src\agent_host\registry.rs'
Assert-NonEmptyFile $registryPath 'goose 解析链 registry.rs'
$registryText = Get-Content -LiteralPath $registryPath -Raw -Encoding UTF8
Assert-Contains $registryPath $registryText 'verify_downloaded_goose' '解析链必须在启动前复算已下载 goose 的 sha256'
Assert-Contains $registryPath $registryText 'goose_install_state' '解析链必须上报 NotInstalled 供 UI 引导下载'
Assert-Contains $registryPath $registryText 'pub const MIN_GOOSE_VERSION: &str = bundle::GOOSE_VERSION;' '版本门槛必须复用 bundle::GOOSE_VERSION（防止 UI 与启动门漂移）'

# --- 3. 许可与来源说明仍在 ------------------------------------------------------
# 防的是：goose 不再内嵌后，许可/来源说明被「顺手删掉」——发行包里没有任何一句说明
# goose 从哪来、什么许可、校验到什么程度。三条文案必须与代码里的 spec/版本门槛一致。
$noticePath = Join-Path $repo 'NOTICE'
Assert-NonEmptyFile $noticePath 'duthub NOTICE'
$noticeText = Get-Content -LiteralPath $noticePath -Raw -Encoding UTF8
Assert-Contains $noticePath $noticeText 'goose' 'NOTICE 必须保留 goose 段'
Assert-Contains $noticePath $noticeText 'https://github.com/aaif-goose/goose' 'NOTICE 必须给出 goose 上游来源'
Assert-Contains $noticePath $noticeText 'Apache' 'NOTICE 必须写明 goose 的 Apache 许可'
Assert-Contains $noticePath $noticeText 'does not bundle' 'NOTICE 必须明确 duthub 不捆绑 goose'
Assert-Contains $noticePath $noticeText 'checksumVerified: false' 'NOTICE 必须说明未固定校验的语义'
# 防的是：NOTICE 回退成「duthub embeds/redistributes goose」的旧陈述——新发布物里没有
# goose 字节，这种声明属于虚假陈述。
Assert-NotContains $noticePath $noticeText 'duthub embeds' 'NOTICE 不得再宣称内嵌/再分发 goose'
Assert-NotContains $noticePath $noticeText 'Bundled third-party component' 'NOTICE 不得再宣称捆绑 goose'

Assert-NonEmptyFile $provenanceFile 'goose 来源说明 goose-distribution.md'
$provenanceText = Get-Content -LiteralPath $provenanceFile -Raw -Encoding UTF8
Assert-Contains $provenanceFile $provenanceText 'not bundled' '来源说明必须明确 goose 不随发布物分发'
Assert-Contains $provenanceFile $provenanceText $gooseUrl '来源说明必须与 TOOL_GOOSE 的下载 URL 一致'
Assert-Contains $provenanceFile $provenanceText 'Apache-2.0' '来源说明必须写明 Apache-2.0'
Assert-Contains $provenanceFile $provenanceText 'sha256' '来源说明必须记录 sha256 口径'
Assert-Contains $provenanceFile $provenanceText 'checksumVerified: false' '来源说明必须说明未固定校验的语义'
Assert-Contains $provenanceFile $provenanceText $gooseVersion '来源说明必须写明版本门槛（与 bundle.rs GOOSE_VERSION 一致）'

$adrPath = Join-Path $repo 'docs\decisions\ADR-3-GOOSE-BUNDLE.md'
Assert-NonEmptyFile $adrPath 'ADR-3'
$adrText = Get-Content -LiteralPath $adrPath -Raw -Encoding UTF8
Assert-Contains $adrPath $adrText '2026-11-25 改为下载分发' 'ADR-3 必须保留下载分发章节（取代内嵌资产口径）'
Write-Host "goose distribution contract ok: url=$gooseUrl version>=$gooseVersion"

# --- 4. Authenticode（社区工具，不自写签名链解析） ------------------------------
if (-not (Get-Command Get-AuthenticodeSignature -ErrorAction SilentlyContinue)) {
    Stop-Gate "Get-AuthenticodeSignature 不可用；请在 Windows PowerShell/pwsh 上运行发布门"
}

$duthubExe = Join-Path $dist 'duthub.exe'
Assert-File $duthubExe 'dist/duthub.exe'
$duthubSignature = Get-AuthenticodeSignature -LiteralPath $duthubExe
$duthubStatus = [string]$duthubSignature.Status
Write-Host "duthub.exe Authenticode: $duthubStatus"
if ($duthubStatus -ne 'Valid' -or $null -eq $duthubSignature.SignerCertificate) {
    if ($AllowUnsigned.IsPresent) {
        # Honest degradation, not a claim of validity: the release notes below
        # record the real Authenticode status, and tag builds never reach here
        # unsigned because they do not pass -AllowUnsigned.
        Write-Host "::warning::dist/duthub.exe 未通过 Authenticode 校验（Status=$duthubStatus）；这是非发布构建（-AllowUnsigned），发布仍需签名"
    } else {
        Stop-Gate "dist/duthub.exe 未通过 Authenticode 校验（Status=$duthubStatus）。发布构建必须先用受信任代码签名证书签名（signtool）；请配置签名证书后重跑。"
    }
}
if ($null -ne $duthubSignature.SignerCertificate) {
    Write-Host "duthub.exe signer: $($duthubSignature.SignerCertificate.Subject)"
}

# goose 不再随发布物分发，因此这里没有「goose 签名状态」可断言。发布说明与来源说明
# 不得对未验证的程序宣称 NotSigned（那是伪造结论）；bundle.rs 的
# authenticode_status() 返回「未内置（随上游发行包）」由该文件的断言与 Rust 单测覆盖。

# --- 5. 发布物旁附声明 ----------------------------------------------------------
# 防的是：发布包只带 exe、不带任何许可/来源说明；或旁附的 PROVENANCE 与仓库源不一致
# （发布物里说一套、仓库里写一套）。旧口径的 goose LICENSE/NOTICE/manifest 不得残留，
# 否则等于继续宣称「随包分发 goose」。
$duthubLicenseOut = Join-Path $dist 'licenses\duthub\LICENSE'
$duthubNoticeOut = Join-Path $dist 'licenses\duthub\NOTICE'
$gooseProvenanceOut = Join-Path $dist 'licenses\goose\PROVENANCE.md'
foreach ($name in @('LICENSE', 'NOTICE', 'manifest.json')) {
    $stale = Join-Path $dist ('licenses\goose\' + $name)
    if (Test-Path -LiteralPath $stale) {
        Stop-Gate "发布物仍旁附内嵌口径的 goose $name：$stale；新口径只允许 licenses/goose/PROVENANCE.md（ADR-3 §9）"
    }
}
Assert-SameFile (Join-Path $repo 'LICENSE') $duthubLicenseOut 'duthub LICENSE'
Assert-SameFile (Join-Path $repo 'NOTICE') $duthubNoticeOut 'duthub NOTICE'
Assert-SameFile $provenanceFile $gooseProvenanceOut 'goose 来源说明'

# --- 6. SBOM 与 audit -----------------------------------------------------------
$sbomJson = Read-JsonFile $sbom 'CycloneDX SBOM'
if ((Get-JsonValue $sbomJson 'bomFormat') -ne 'CycloneDX') {
    Stop-Gate "SBOM bomFormat 必须是 CycloneDX：$sbom"
}
if ((Get-JsonValue $sbomJson 'specVersion') -ne '1.7') {
    Stop-Gate "SBOM specVersion 必须是 1.7（实际 $(Get-JsonValue $sbomJson 'specVersion')）"
}
$components = Get-JsonValue $sbomJson 'components'
if ($null -eq $components -or @($components).Count -eq 0) {
    Stop-Gate "SBOM 没有 components：$sbom"
}
$metadata = Get-JsonValue $sbomJson 'metadata'
$topComponent = Get-JsonValue $metadata 'component'
$topName = [string](Get-JsonValue $topComponent 'name')
if ([string]::IsNullOrWhiteSpace($topName) -or $topName -notlike '*duthub*') {
    Stop-Gate "SBOM metadata.component.name 不像 duthub 包（实际 '$topName'）"
}
Write-Host "sbom ok: $sbom ($(@($components).Count) components, top=$topName)"

$auditJson = Read-JsonFile $audit 'duthub cargo-audit JSON'
Assert-AuditClean $auditJson 'duthub cargo-audit'
Assert-AuditExit $auditExit $audit 'duthub cargo-audit'

# goose 运行时依赖审计（占位，显式未验证）：duthub 不再分发 goose，发布物里没有
# goose 的 Cargo.lock，无法也不该由本仓库生成它的 SBOM/审计。workflow 必须写一份以
# UNVERIFIED 开头的状态文件，本脚本断言它存在——这条检查不允许被静默删除；如果
# owner 以后补了上游 Cargo.lock 的审计结果（dist/sbom/audit-goose.json + exit 文件），
# 那它必须干净且退出码 0，不能拿一份有漏洞的审计来充数。
Assert-NonEmptyFile $gooseAuditStatus 'goose 运行时依赖审计状态占位（workflow 写入）'
$gooseAuditStatusText = (Get-Content -LiteralPath $gooseAuditStatus -Raw -Encoding UTF8).Trim()
if ($gooseAuditStatusText -notmatch '^UNVERIFIED') {
    Stop-Gate "goose 审计状态占位必须以 UNVERIFIED 开头（明示未验证）：$gooseAuditStatus"
}
Assert-Contains $gooseAuditStatus $gooseAuditStatusText 'ADR-3' 'goose 审计状态占位必须指向 ADR-3 §9 的解释'
if (Test-Path -LiteralPath $gooseAudit -PathType Leaf) {
    Assert-File $gooseAuditExit 'goose cargo-audit 退出码（提供了审计结果就必须提供退出码）'
    $gooseAuditJson = Read-JsonFile $gooseAudit 'goose cargo-audit JSON'
    Assert-AuditClean $gooseAuditJson 'goose cargo-audit'
    Assert-AuditExit $gooseAuditExit $gooseAudit 'goose cargo-audit'
} else {
    Write-Host "goose runtime dependency audit: UNVERIFIED (placeholder asserted; no goose bytes are distributed)"
}

# --- 7. SHA256SUMS --------------------------------------------------------------
# 覆盖发布物实际携带的文件：exe、duthub 许可/声明、goose 来源说明、duthub SBOM/audit
# 与 goose 审计状态占位。已删除的 goose LICENSE/NOTICE/manifest 不再列入（它们不存在
# 于新发布物，列进去只会让门禁永久红）。
$checksumTargets = @(
    'duthub.exe',
    'licenses/duthub/LICENSE',
    'licenses/duthub/NOTICE',
    'licenses/goose/PROVENANCE.md',
    'sbom/duthub.cdx.json',
    'sbom/audit.json',
    'sbom/audit-exit.txt',
    'sbom/goose-audit-status.txt'
)
$checksumLines = foreach ($relative in $checksumTargets) {
    $path = Join-Path $dist ($relative -replace '/', [System.IO.Path]::DirectorySeparatorChar)
    Assert-File $path "SHA256SUMS 目标 $relative"
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $relative"
}
$checksumsDir = Split-Path -Parent $checksums
if (-not [string]::IsNullOrWhiteSpace($checksumsDir)) {
    New-Item -ItemType Directory -Force -Path $checksumsDir | Out-Null
}
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[System.IO.File]::WriteAllText($checksums, (($checksumLines -join "`n") + "`n"), $utf8NoBom)
Write-Host "checksums written: $checksums"

# --- 8. Release notes（版本/哈希/goose 下载分发事实） --------------------------
# 防的是：发布说明继续写「goose-acp.exe <hash> / NotSigned」——发布物里根本没有这个
# 文件，那是伪造可核对性。goose 段只允许陈述事实：未捆绑、来源 URL、许可、版本门槛、
# 未固定校验（checksumVerified: false）与运行时复算。
$workspaceManifest = Get-Content -LiteralPath (Join-Path $repo 'Cargo.toml') -Raw -Encoding UTF8
$workspaceVersion = 'unknown'
if ($workspaceManifest -match '(?ms)\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    $workspaceVersion = $Matches[1]
}
$duthubHash = (Get-FileHash -LiteralPath $duthubExe -Algorithm SHA256).Hash.ToLowerInvariant()
$notesDir = Split-Path -Parent $notes
if (-not [string]::IsNullOrWhiteSpace($notesDir)) {
    New-Item -ItemType Directory -Force -Path $notesDir | Out-Null
}
$releaseNotes = @"
duthub $workspaceVersion

SHA-256
-------
duthub.exe                    $duthubHash

goose (not bundled; installed by the user)
------------------------------------------
No goose bytes are shipped with this release. goose is installed on demand from
the in-app "tool downloads" page (duthub-tooldown) from:
  $gooseUrl
License: Apache-2.0. Minimum version: $gooseVersion.
The rolling upstream asset is not pinned: the installer records the measured
SHA-256 in <config dir>/tools/goose/manifest.json with checksumVerified: false,
and the runtime re-hashes the installed executable against that record before
launch. goose runtime dependencies are UNVERIFIED at release time (ADR-3 §9).
See licenses/goose/PROVENANCE.md and sbom/goose-audit-status.txt.

Authenticode
------------
duthub.exe                    $duthubStatus
goose                         not verified by duthub (not bundled; no signature claim)

See sbom/ for the CycloneDX SBOM and cargo-audit report of duthub itself.
SHA256SUMS covers duthub.exe, the shipped licence/provenance files and the sbom
files in this release.
"@
Assert-Contains $notes $releaseNotes 'checksumVerified: false' '发布说明必须保留 goose 未固定校验的事实'
Assert-Contains $notes $releaseNotes 'UNVERIFIED' '发布说明必须标明 goose 运行时依赖未验证'
Assert-NotContains $notes $releaseNotes 'goose-acp.exe' '发布说明不得再声明已删除的内嵌 goose 资产'
[System.IO.File]::WriteAllText($notes, $releaseNotes, $utf8NoBom)
Write-Host "release notes written: $notes"

Write-Host "RELEASE GATE OK"
exit 0
