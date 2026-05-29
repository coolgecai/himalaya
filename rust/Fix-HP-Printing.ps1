# Fix-HP-Printing.ps1
# --------------------------------------------------------------
#  脚本为 Windows PowerShell 环境编写，请右键以 “以管理员身份运行” 方式执行。
#  功能：
#   1️⃣ 自动安装/启用 Microsoft Print PDF（若未安装）。
#   2️⃣ 检查并启动 Print Spooler Service。
#   3️⃣ 卸载并重新安装 HP 驱动程序（需要先准备好最新的 INF 包路径）。
#   4️⃣ 列出所有已注册打印机并显示状态，帮助确认修复效果。
# --------------------------------------------------------------

# ------------------- 1. 启用 Microsoft Print PDF -------------------
Write-Host "=== 检查/安装 Microsoft Print PDF ==="
if (-not (Get-WindowsFeature -Name "MicrosoftPrintToPDF" -ErrorAction SilentlyContinue)) {
    Write-Host "[*] Microsoft Print PDF 未安装，正在安装..."
    # DISM 需要管理员权限
    DISM /Online /Enable-Feature /FeatureName:MicrosoftPrintToPDF /All | Out-Null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "[+] Microsoft Print PDF 安装成功。"
    } else {
        Write-Warning "[-] 安装 Microsoft Print PDF 时出现错误，请手动通过「设置」→「可选功能」安装。"
    }
} else {
    Write-Host "[+] Microsoft Print PDF 已经存在。"
}

# ------------------- 2. 检查并启动 Print Spooler -------------------
Write-Host "`n=== 检查 Print Spooler Service ==="
$spooler = Get-Service -Name Spooler -ErrorAction SilentlyContinue
if ($null -eq $spooler) {
    Write-Warning "服务 'Spooler' 未找到。请确认系统完整性，或在「服务」管理控制台手动启动。"
} elseif ($spooler.Status -ne 'Running') {
    Write-Host "[*] Spooler 正在停止...（如果已运行则跳过}"
    try {
        Stop-Service -Name Spooler -Force -ErrorAction Stop
        Start-Service -Name Spooler
        Write-Host "[+] Print Spooler 已启动。"
    } catch {
        Write-Warning "启动 Spooler 时出现错误: $_"
    }
} else {
    Write-Host "[+] Print Spooler 正常运行。"
}

# ------------------- 3. 重新安装 HP 激光驱动 -------------------
Write-Host "`n=== 处理 HP 驱动程序 ==="
# 假设最新的 HP INF 包位于 C:\HP\Drivers（请根据实际路径修改）
$hpDriverFolder = "C:\HP\Drivers"
if (Test-Path $hpDriverFolder) {
    Write-Host "[*] 检测到驱动文件夹: $hpDriverFolder"
    # 获取所有 *.inf 文件（通常 HP 驱动包含多个 INF）
    $infs = Get-ChildItem -Path $hpDriverFolder -Filter "*.inf" | Select-Object -First 1
    if ($infs) {
        Write-Host "[*] 尝试使用驱动: $($infs.FullName)"
        # 添加打印机（如果系统中已经存在同名打印机则先删除）
        $existing = Get-Printer -Name "HP_LaserJet" -ErrorAction SilentlyContinue
        if ($existing) {
            Write-Host "[*] 检测到旧的 HP 打印机，正在卸载..."
            Remove-Printer -Name $existing.Name -ErrorAction Stop
            Write-Host "[+] 旧驱动已删除。"
        }
        # 添加新打印机并指定驱动 INF
        try {
            Add-Printer -ConnectionName "HP_LaserJet" `
                        -VendorName "Hewlett-Packard" `
                        -DriverName $infs.BaseName `
                        -ErrorAction Stop | Out-Null
            Write-Host "[+] HP 打印机已成功添加并设为默认。"
        } catch {
            Write-Warning "添加 HP 打印机时出错：$_"
        }
    } else {
        Write-Warning "[-] 未在 $hpDriverFolder 中找到 *.inf 驱动文件，请下载并解压最新的 HP 驱动，放入该目录后再运行脚本。"
    }
} else {
    Write-Warning "[-] 指定驱动文件夹不存在: $hpDriverFolder`n   请先在 https://support.hp.com 上下载对应你的 LaserJet/DeskJet 等型号的 Windows 驱动，解压后将 *.inf 文件放入该文件夹。"
}

# ------------------- 4. 列出当前已注册打印机状态 -------------------
Write-Host "`n=== 当前所有已注册的打印机 ==="
Get-Printer | Format-Table Name, PrinterStatus, DriverName -AutoSize

Write-Host "`n脚本执行完毕。如果仍有问题，请检查上述警告信息并手动完成以下事项："
Write-Host "- 在「设置」→「设备」→「打印机和扫描仪」里确认 HP 打印机显示为「就绪」。"
Write-Host "- 若仍显示错误，请在「设备管理器」中卸载有问题的驱动后重新安装。"
Write-Host "- 可以访问 https://support.hp.com/ 链接下载最新的 Windows 驱动并手动更新。"