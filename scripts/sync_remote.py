import paramiko

ssh = paramiko.SSHClient()
ssh.set_missing_host_key_policy(paramiko.AutoAddPolicy())
ssh.connect('8.218.197.39', username='root', password='3442441682Yht')
cmd = 'cd /opt/gemini-qq-bot && git pull origin main && cargo build --release --bin gemini-qq-bot && systemctl restart gemini-qq-bot'
stdin, stdout, stderr = ssh.exec_command(cmd)
print(stdout.read().decode('utf-8'))
err = stderr.read().decode('utf-8')
if err:
    print('BUILD LOG:', err)
ssh.close()
