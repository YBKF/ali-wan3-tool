cargo build --release
Remove-Item -Path ".\target\release\万相 3.0 视频生成工具.exe" -Force
Rename-Item -Path .\target\release\ali-wan3-tool.exe -NewName "万相 3.0 视频生成工具.exe"