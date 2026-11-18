# 一键挂载本地Windows电脑至云端小简（关掉窗口即自动断开回退纯云端模式）
Write-Host "正在启动本地Windows电脑外接桥接节点并挂载至云端小简..." -ForegroundColor Cyan
python "$PSScriptRoot\scripts\local_pc_bridge.py"
