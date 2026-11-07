param(
    [string]$Message = "chore: update gemini-qq-bot codebase"
)

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

Write-Host ">>> 正在提交本地修改至 Git..."
git add .
$status = git status --porcelain
if ($status) {
    git commit -m "$Message"
} else {
    Write-Host ">>> 本地工作区无未提交更改。"
}

Write-Host ">>> 正在推送到 GitHub 远端仓库..."
git push origin main

Write-Host ">>> 正在通知云端服务器拉取更新并重新编译..."
python -c "
import paramiko
ssh = paramiko.SSHClient()
ssh.set_missing_host_key_policy(paramiko.AutoAddPolicy())
ssh.connect('8.218.197.39', username='root', password='3442441682Yht')
cmd = 'cd /opt/gemini-qq-bot && git pull origin main && cargo build --release --bin gemini-qq-bot && systemctl restart gemini-qq-bot'
stdin, stdout, stderr = ssh.exec_command(cmd)
print(stdout.read().decode('utf-8'))
err = stderr.read().decode('utf-8')
if err: print('LOG:', err)
ssh.close()
"

Write-Host ">>> 同步与云端部署已全部完成！"
