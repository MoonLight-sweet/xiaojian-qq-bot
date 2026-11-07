param(
    [string]$Message = "chore: update codebase"
)

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

Write-Host ">>> Staging changes..."
git add .
$status = git status --porcelain
if ($status) {
    git commit -m "$Message"
} else {
    Write-Host ">>> No unstaged changes."
}

Write-Host ">>> Pushing to GitHub..."
git push origin main

Write-Host ">>> Syncing and building on remote cloud server..."
python scripts/sync_remote.py

Write-Host ">>> Sync & Deployment finished successfully!"
