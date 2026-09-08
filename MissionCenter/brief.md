<!-- Generated materialized view. Do not edit directly; rebuild from canonical MissionCenter files. -->
<!-- mission-center-derived schema=1.0 fingerprint-format=sha256-v2-lf source-fingerprint=c30fb26d96fb83dcd4f9474a9e59ceb9f08ef8f9838fc2ba4a1c349c727440bc -->
# 任務簡報

- 最後整理: 2026-09-08
- 來源指紋: `c30fb26d96fb83dcd4f9474a9e59ceb9f08ef8f9838fc2ba4a1c349c727440bc`
- 唯一真實來源: `tasks.md`
- 專案: Codex Mission Center
- 北極星: 將 Mission Center 升級為研究驅動的自適應 Project OS，並提供可選的本機 Live Agent HUD
- 週期: v0.5.1 Repository Seal

## 今日摘要 · 2026-09-08
- 整合 macOS 本機修復：Rust task table 讀寫共用定位、支援中文欄位與多表格續接，focus 讀寫上限一致提高至 16 KiB；大檔替換不再無界讀取舊內容。
- 正式 Rust publisher 要求既有四平台驗證包，排除 Python 相容層與 build 產物；複製使用固定平台路徑並核對 staged checksum，拒絕父目錄 symlink。
- CodeRabbit 第一輪完成 19 檔審查、提出 3 個 Major issues；逐項確認並修正，另補多表格混合續接及驗證後 payload 變動的回歸測試。
- CodeRabbit 第二輪隔離複查 6 檔，提出 1 個 Minor issue：續接列誤以第一欄代替 ID 欄；已重現漏列並改用 header 定位 ID。兩輪共 4 issues 均有修正，保留第三次每小時額度，未宣稱修後另有零問題複查。
- 最後整合測試另重現 macOS HUD 延遲送出 HTTP request 時回應為空：accepted socket 繼承非阻塞模式，現於處理前明確恢復阻塞並保留既有讀寫 timeout；此項是本機測試發現，不冒稱 CodeRabbit 問題。

## 重要護欄 (7)
- GR-001
- GR-002
- GR-003
- GR-004
- GR-005
- GR-006
- GR-007

## 需要時再讀
- 目前工作（0 項）→ `working-set.md`
- 修改任務生命週期／順序 → `tasks.md`
- 查閱理由／證據 → `decisions.md`、`notes.md`、`smoke-tests.md`
- 簡報／工作集過期或截斷 → 執行 `mission_maintenance.py sync` 後再讀 canonical files
