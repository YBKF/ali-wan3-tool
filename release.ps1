cargo build --release
Remove-Item -Path ".\target\release\万相 3.0 视频生成工具.exe" -Force
Copy-Item -Path ".\target\release\ali-wan3-tool.exe" -Destination ".\target\release\万相 3.0 视频生成工具.exe" -Force