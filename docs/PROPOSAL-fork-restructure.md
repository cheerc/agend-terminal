# Fork 重構提案：可自訂角色、共用開發契約

**檔名說明**：進 repo 時存為 `docs/PROPOSAL-fork-restructure.md`。本章節沿用 § 編號不變，引用時以本檔為準。

**目標專案**：`cheerc/agend-terminal`  
**文件性質**：agend-terminal fork 重構的目標設計提案。不是實作授權；每一工作包仍需各自的 spec 與實作授權。

**本文件是自我完備的**：讀者不需要任何其他提案文件即可理解全部設計決策與其理由。早期草稿中的事實與論證已直接寫入本文，不再以外部引用代替。
**日期**：2026-10-04  
**證據基線**：本文的 source 事實核對於 `184a5feff9d530834e44c4324118e0966675494f`。該 commit 為 2026-10-04 fork 時的上游 HEAD；此後上游可能繼續演進，重新實作前須以選定 base 重驗 load-bearing 事實。  
**範圍**：產品定位與保留邊界、官方 workflow 與 instruction／skill 契約、派工到結案的閉環、no-CI 完整生命週期、恢復與跨 backend 邊界、治理與 decision board、fork／部署／回復、GUI、驗收與 downstream 減法。  
**審查與裁決的落點**：§1.3 集中記錄全部 operator 裁決；每次 review 的 verdict、發現與修訂理由記在對應 PR 的審查簽收與 commit 歷史。

## 0. 閱讀方式與證據邊界

先讀 §1 產品定位與 operator 裁決，再讀 §2–§12 的設計主張，最後讀 §13–§15 的執行、驗收與 downstream 減法。附錄 A 是上游 open issue 的處置追蹤入口。

本文使用三種標記：

- **[O]**：operator 已明示的需求或裁決。只在 §1 集中記錄，正文引用 ID。
- **[P]**：本文提出的設計建議；等待 review 與 operator 接受，不宣稱已實作。
- **[E]**：讀到的文件或工具契約。標 **[S]** 者為 source 層事實（已對 `184a5fe` 核對）；標 **[T]** 者為靜態推論，未端到端驗證；標 **[D]** 者為 operator 決策。

本提案對照了兩類來源：產品自身的 configuration／fleet／skills／architecture 文件，以及 fork 之前在 daemon 之外自行維護的角色指示與工作流程文件（該套文件不在本 repo 內，是「每個使用者都得自己重新累積」的成本來源，本提案的 §2 就是要消除這個負擔）。生產資料統計、匿名 caller 請求、跨 backend 測試與 daemon source 全面複驗未重跑。

本文件是完整的**目標設計提案**。歷史事故與逐行 source 索引留在 git 歷史；本檔不重述審查過程與修正史。凡曾出現互相矛盾的論述，本檔直接選定目標行為並說明其理由，不以「後面有一段更正」要求實作者自行仲裁。

**本文件類別**：本文件屬 repo 的 foldable 類別（`docs/PROPOSAL-*.md`）。依 `CONTRIBUTING.md` 與 `tests/docs_bilingual_invariant.rs`，foldable 文件**豁免**雙語姊妹檔與 `docs/README.md` 索引（因它是作者自用、實作完成後會被 fold 的暫時性文件），但**位置必須直接位於 `docs/` 下**，且**新增或修改它的 commit 必須帶 `Approved-by: cheerc` trailer**（由 `.github/workflows/approved-proposal.yml` 檢查）。

## 1. 產品定位與不可偷換的裁決

### 1.1 這個 fork 要成為什麼 [O]

Fleet 像一家公司或小工作室；team 是專案團隊。使用者可以自行定義角色、個性、專長、team 形狀與 model/backend 組合；共同的開發流程由 agend-terminal daemon 提供可理解、可操作、可恢復的契約。

Backend CLI 繼續負責單一 agent 的模型互動、工具迴圈及廠商自身 harness。agend-terminal 負責把這些 agent 組成能協作的 team，**不重做廠商 CLI、不接管它的推理迴圈**。

從本輪起，產品目標是 `cheerc/agend-terminal`；不再以「要不要 fork」或「原作者是否停止維護」作為前置問題。原作者的未來維護狀態沒有在本輪查證，也不是 fork 的必要論據。

### 1.2 延續的保留邊界 [O]

保留 tmux 工作環境、多 backend 原生 CLI、task／decision board、CI/CD 與新的 no-CI 模式、可自訂 instructions、Telegram 既有架構（V6-7：功能需求重評估先不動，遇問題另開 issue），以及輔助管理 GUI。**issue 一律開在本 repo（V6-8）；部署用 binary 一律從本 repo 的 `main` build（V6-9）**。GUI 不變成 IDE/ADE；Telegram 本輪不重構。減少 protocol 與 MCP 使用歧義是核心需求，不再列為「之後再想」。

### 1.3 既有裁決的唯一總表 [O]

| ID | 已裁決內容 | 本檔落點 |
|---|---|---|
| P1 | 治理者可協助 task update／結案，但保留原 owner；`assignee` 不因治理權限而開放改寫 | §8 |
| P2 | 先修 Active 下 anonymous／未知 caller 的全開 fallback，再建立治理能力 | §8、§13 W1 |
| A-1 | 先允許直接聯絡任何 team member，不要求 allow 清單或只能找 lead；未來收緊另議 | §7、§8 |
| A-2 | no-CI 在 fleet.yaml 宣告，可切換，不要求防 agent 改檔；放行 CI 條件時必須宣告「非實測」 | §6 |
| V6-1 | （2026-10-04，補充 A-2）no-CI 宣告**涵蓋一切 CI deficit，包含 current HEAD 上真實跑完的 failed**。原話：「no-CI 就是 operator 在目前的開發流程當中，不需要啟用 daemon ci 相關功能……使用者可以隨時決定此 team 現階段到底要不要導入 ci／使用 daemon ci 相關功能」。成因不限於額度用完；不做「是否真的執行過」的分類器 | §6 |
| V6-2 | （2026-10-04）per-(tool, action) grants 模型與 GUI「勾選 MCP 權限」延到 v1 之後；v1 只做治理基礎（P1、A-1、A-6、commander creator-scoped） | §8、§11、§13 |
| V6-3 | （2026-10-04）instructions 注入、protocol 內容、哪些放 daemon／哪些允許客製化，乃至整體結構，**留到寫 spec 時單項詳細討論**；本檔 §2.3–§4 只是候選方向，不是已定案設計。現況參考：`.claude/agend.md` 前段是 daemon 預設注入，使用規範集中在 `FLEET-DEV-PROTOCOL.md` | §2、§3、§4、§13 W2 |
| V6-4 | （2026-10-04）v1 = 現有 customization 仍安裝著、可用來開發 cheerc fork 的第一版；downstream **逐步退役，不一次到位**。理由：「我們都還是透過 daemon 在工作，改壞了要能夠退 daemon 版本，重新工作才行」 | §12.3、§13、§15 |
| V6-6 | （2026-10-04）`review-class correction` 的授權對象為**「具權者」**，不維持 operator-only；「具權者」資格依 §8.3 的 grant／role 上限定義，spec 決定具體集合 | §8.4、§13 W5 |
| V6-7 | （2026-10-04）Telegram 保留架構不變；**「重新評估功能需求」原則上先不動**，遇到問題時另開 issue 處理，不列為 v1 或後續版本的交付項 | §1.2、§15.3 |
| V6-8 | （2026-10-05）**所有 issue 一律開在本 repo**（`cheerc/agend-terminal`）：新的 daemon／MCP bug、feature request 一律在此開或查；上游既有 issue／PR 連結只作歷史 provenance，不機械改寫。§12.1 的上游 open issue 接收規則仍適用（收進來的在本 repo 重開並連回原 issue） | §1.2、§12.1、附錄 A |
| V6-9 | （2026-10-05）**部署用 binary 一律從本 repo 的 `main` build**，不再使用 current-build 或其他部署分支。已部署 identity 以 running binary 的 build SHA 為準，且須為 `origin/main` 的祖先或相等 | §1.2、§10.2、§12.1 |
| V6-5 | （2026-10-04）**downstream 移植到 upstream 的第一個穩定版本**，其判準是：我們所有 fleet.yaml 的 instance **不需要指定 `instructions:`，只給 `role:` 就能吃到 daemon 預設值**。即 daemon 依 role 提供預設 instructions；`instructions:` 退為可選覆寫。不是 v1 的要求（v1 依 V6-4 仍沿用現行 L1） | §2.2、§13 W8、§15 |
| A-4 | Draft、InsufficientVerified 不降級；保留例外 force 路徑，且不能假設 fleet 擁有 forge merge 權限 | §5、§6、§8 |
| A-5 | 結構化 refs 與 close／封存進入 v1；新關聯及失效決策應能正常整理 | §9 |
| A-6 | sweep／board_sweep／board_unretire 的授權是可設定清單，預設 general + operator | §8 |
| A-7 | 救援者不改 plan 治理權威 | §8 |
| A-8 | 不以新治理權限覆蓋 decision 的既有 update／answer／archive ACL | §9 |
| A-9 | 外部 client 必須自報名稱，顯示為 `external-agent:<name>`；標籤不是身分保證 | §8、§11 |
| A-10 | GUI 與 daemon 同 repo、獨立發布；daemon 不引入 web framework | §11 |

A-3 已被併入 A-8，不重建該裁決。**`close` 已存在**：`decision(action="update", archive=true)` 即可單筆封存（`decisions.rs` 的 `update` 路徑，ACL 為 `can_mutate_decision`＝author ＋ orchestrator）。本提案沿用它，不要求重新實作已有的 close；封存後從白板消失、且不能再當治理決策，正是「失效決策下線」的既有語意。只有 `reopen` 需新增（§9）。

### 1.4 本次新增設計的批准邊界 [P]

§2–§15 中超出上述裁決的內容是 **本提案的推薦方案**。其中影響較大的新增範圍是：官方 workflow package、可查詢的有效契約、dispatch／completion 收斂、instruction／skill activation 證據、cold-start scenario 驗收。接受本提案不等於逐一批准尚未存在的 implementation plan；後續仍需按工作包形成 spec／plan。

依 V6-3：上述「官方 workflow package、可查詢的有效契約、instruction／skill activation」中，凡涉及注入結構與 protocol／客製化分界者，降為 spec 階段的討論輸入，不在本檔定案。v1 的實際交付範圍以 §13 的「v1 最小切面」欄為準（V6-4）。

## 2. General 的判斷：上收產品契約，不上收我們的組織圖

### 2.1 現有文件契約還缺哪一層

daemon 已有清楚的 handler、ACL 與 gate 契約，但仍缺「一個沒有任何私有記憶、沒有自訂 skill 集合、甚至沒有預設角色名的 instance，怎樣正確完成工作」的產品契約。

具體例子 [E]：

- downstream `instructions/lead.md:38-39` 要重講 create 不等於 dispatch、同一 task identity 與 binding 對帳；這是一般工具工作流，不是 lead 的個性。
- `docs/team-workflow-map.md:69-70` 要區分 disposition、task settlement 與 inbox settlement；這是生命週期，不該每家公司重新發明。
- `skills/handoff/SKILL.md:23-28` 明示：沒有 trigger，交接檔就不會自動被讀。只交付「一份好 skill」不等於恢復機制。
- `README.md:44-57` 的 L1 spawn-time、skill 下次讀取、手工 symlink 與兩波 cutover，顯示 customization 的成本包括**載入與生效**，不只是寫 prompt。
- 舊提案曾提議 `disable_shared_block`（**該欄位在 source 中完全不存在**，從未實作）；若關掉整包，identity、inbox／channel、recovery 誰負責並未閉合。

### 2.2 三條路與推薦

| 路線 | 好處 | 代價 | 判斷 |
|---|---|---|---|
| 只修既有 handler，下游照舊補 SOP | 改動較集中 | 新使用者仍需自行發現跨工具規則與載入陷阱 | 不足以滿足本輪目的 |
| 把現有 General／Lead／Impl／Reviewer 全包成 daemon 唯一流程 | 很快能複製我們的工作方式 | 固定組織、特定名字與 backend；變成另一份肥大 protocol | 不採用 |
| **薄的 daemon 核心契約 + 官方 phase-based workflow + 自訂角色／專案政策** | 通用流程由產品維護，角色仍自由 | 需補 effective-policy、activation、state 收斂與相容性測試 | **推薦** |

**V6-5 的界定**：上表第二列不採用的是「把我們的角色寫死成 daemon **唯一**流程」，不是「daemon 不提供角色預設」。operator 的目標狀態是 daemon 依 `role:` 提供預設 instructions、使用者可用 `instructions:` 覆寫；兩者相容。spec 需決定：以自由文字 `role` 還是 typed `role_kind` 選預設（§3.2：選預設 instructions 不等於取得 authority）、未知 role 的行為，以及預設內容與 V6-3 結構討論的關係。

這不是新增通用 workflow engine、任意 DAG 語言或全能 plugin 平台。v1 只整理目前已存在的 task／dispatch／review／CI／merge／recovery 路徑，讓它們有一個共同的可用契約。

### 2.3 單一 authority 的責任表 [P]

> V6-3：本表的 daemon／官方 workflow／L1／skill 分界是候選方向，於 spec 階段與 §4 一併單項討論，不是定案。

| 層 | 必須擁有 | 不應擁有 |
|---|---|---|
| daemon runtime | identity／authority 檢查、binding、generation、持久義務、狀態轉換、freshness、可恢復副作用 | 專案的測試指令、角色人格、自然語言品質判斷 |
| MCP／CLI 公開介面 | action 輸入／輸出、原子性邊界、拒絕原因、可重試性與合法下一步 | 整本開發 SOP、歷史事故故事 |
| 官方 workflow package | 跨工具的 phase、交接、角色責任槽、正常／錯誤／恢復路線 | 個別使用者的組織名稱與專案路徑 |
| 使用者 fleet／team policy | 人員配置、工具權限、治理名單、CI mode、已支援的 workflow 選項 | 不存在的 daemon capability |
| 角色 L1 | 角色任務、語氣、專長、scope、在該組織的責任分工與加嚴規則 | 重複 MCP 欄位、手抄 daemon state machine |
| 專案 baseline | repo／branch／stack／測試／部署／project-local acceptance 與允許的政策 | 改寫 runtime hard gate |
| optional skills | 特定方法與 artifact 品質，例如 brainstorming、review 方法、文件維護 | 未經宣告的額外派工、merge 或 restart 權限 |
| memory／歷史記錄 | 個別教訓、不可重建的人類脈絡、設計原因 | 一般使用者必須知道的正常操作規則 |

產品內可有同一契約的文件與 MCP 呈現，但**不是兩份獨立可寫 authority**。機械欄位由共享型別／metadata 驅動；跨工具語義由一份官方 workflow body 擁有。不要靠逐字 grep 固定散文。

## 3. 官方 workflow：固定責任，不固定角色名字 [P]

### 3.1 最小流程核心

官方 package 必須涵蓋以下正常流程及失敗出口：

`intake → scope/acceptance freeze → dispatch → implement → PR ready → review → convergence → merge readiness → merged observation → closure`

這是**責任與事件序列**，不是強迫每段各開一個 agent，也不是由 daemon 自動執行全部決策。所有 mutation 仍經現有 daemon primitive；官方 skill 只是正確使用它們的第一方操作契約。

- PR ready 允許立即 static review，不把 CI green 當唯一派審入口。
- finding、CI event 是 state signal；重新修改交付 HEAD 需要由該工作的 execution coordinator 派 rework。
- exact HEAD、base、assignment generation 變動時，依 gate 重新驗證；不重用過期 receipt。
- ready-for-merge 不等於已 merge，不等於有 forge merge 權限。
- merged observation 只完成對應 subject 的工作；parent delivery 尚有 acceptance／cleanup 時不得自動冒充全部結案。
- research、spec-only、外部 PR、runtime evidence continuation 有明確窄入口，不強塞進 PR-producing branch flow。

### 3.2 責任槽與組織彈性

最少區分：需求發起者、execution coordinator、assignee／author、reviewer、merge actor、parent closure owner。用既有 task、team、assignment identity 表達；不要另造一套人員資料庫。

使用者可以選擇：

- operator 直接找 execution coordinator；
- commander 先做 spec，再交 coordinator；
- General 管多個 project team；
- 小 team 合併部分職能。

**合併職能不合併已裁決的 review 獨立性**。只有一位 reviewer 時，建立工作前選擇符合政策的 single；dual 需要 distinct reviewers。人員後來不足則明示 deficit 與合法 correction／補位路徑，不悄悄降級、不用 force 當常態。v1 不新增 zero-review lane。

`role` 自由文字與 typed `role_kind`／grants 不混同：改名字或寫「你是 CTO」不產生 authority。官方 starter 以可改的名稱示範，而非硬編碼 `general`、`fixup-lead` 等名字作為所有人的流程前提。A-6 指定的預設名單保留（`general` 是**既有預設值**，但依 §3.2 不得作測試對象或 authority 來源），缺少該 instance 時要在設定驗證中提醒。

### 3.3 政策 precedence 必須跨角色一致

建議解析規則：

1. runtime 不變量與 operator 明示的 authority 邊界不能靠散文覆寫。
2. daemon 支援的結構化 policy 依 **project → fleet default → product default** 解析；**CI mode 這一項 policy**：team 層不是第四級 precedence，而是 must-agree constraint：team 宣告只可與解析到的 policy 同向，相反即為 §6.2 第 2 條的 fail-closed（否則階梯會靜默選 project，與 §6.2 的偵測矛盾語義相斥）。其他 policy 類別的 team 層行為留待 spec，本檔不泛化。
3. project baseline 可以補充 acceptance、測試與已允許的 workflow customization。與有效 runtime policy 相反時，回報衝突，不把文字當隱形開關。
4. 角色 L1 可加嚴自身行為、指定如何履行責任；不可透過自稱角色取得別人的權限。
5. 臨時 dispatch 決定本次 scope，不默默覆寫長期 policy；重大 scope 衝突須回原 dispatcher／operator。

這裡需要修的是**共同解析契約**，不是再加一段「有衝突就問」。本輪讀到 Lead L1 把 project policy 當 scoped authority，而 Impl／Reviewer L1 說 project tree「只提供 facts，不是 authority」[E]；這是同一專案可能產生不同閉環判斷的現成例子。本提案要求官方 package 在三種 actor 上使用相同分類，不把這個 downstream 分歧原樣上收。

### 3.4 不全部做成開關

v1 必須支援的是 CI mode、grants、治理名單及官方 workflow 的責任對應。test-first、review 深度、文件模板可作官方預設與 project supplement；不在本次順手建立任意 policy DSL。遇到仍由程式硬編碼的限制，產品要誠實標成「此版不可設定」，不能用 YAML 範例承諾不存在的能力。

## 4. Instructions 與 Skills：從「檔案有了」到「actor 能正確使用」[P]

> **V6-3：本節整體為 spec 階段單項討論的候選輸入，不是定案。** operator 要在寫 spec 時重新討論哪些放 daemon、哪些允許客製化，以及注入／protocol 的整體結構。下文 §4.1 的 kernel 拆層、§4.3 activation、§4.5 protocol 瘦身都是 General 的候選方案；`disable_shared_block` 整包開關**從未實作、不存在於現有 source**也同樣未定。實作前不得以本節任一子節當已批准設計。

### 4.1 注入拆層，取代整包全有／全無（候選方案）

候選方案：以下兩種受支援的產品模式。⚠️ **`disable_shared_block` 在現有 source 中完全不存在**（從未實作），因此這是**從零設計**，不是改造既有開關；現況是 `instructions.rs` 無條件寫入 marker block（`AGEND_BLOCK_START/END`，`src/instructions.rs:94-95`）。欄位名稱留待 schema 設計：

- **標準模式**：小型 coordination kernel + 官方 workflow 的入口索引 + 使用者 L1。
- **自訂 workflow 模式**：相同 coordination kernel + 使用者提供的 workflow entry + 使用者 L1。必須宣告與本版 daemon 的必要契約相容；不假裝自訂文字可關閉 runtime gate。

Kernel 只含：instance／workspace identity、已連上的 tool 入口、訊息種類與正確回覆路線、恢復／交接入口、有效 policy 的查詢入口。不包含完整開發 SOP、個人組織偏好、事故紀錄或整份 protocol。

**本提議調整的是「整包關閉預設注入」這個架構建議方向，不撤銷 operator 的 customization 需求**：使用者可取代官方 workflow／角色文字，但一般 managed team 不應因關掉預設文案而同時失去 inbox、channel 與 recovery 的唯一入口。若另設完全不注入模式，須標成 unmanaged／自行整合，不宣稱仍具標準 team-workflow 保證；v1 不必新增該模式。

### 4.2 不覆寫使用者檔案

- daemon 只寫自己擁有的 generated file／marker range；role instructions 永遠有獨立 user-owned source。
- 既有 `.claude/agend.md` 或共享 `AGENTS.md`／其他 backend 檔案有非 managed 內容時，遷移先辨識與保留；不能用「這檔通常由 daemon 寫」作為刪使用者內容的理由。
- marker 缺失、重複、損壞或 ownership 不明時，回可操作衝突；不重寫整份、不自行把未知內容當垃圾。
- 相同 workspace 的多 instance 不得互覆 identity block；必要時分離 instance-local generated path，或明確拒絕不支援的共享配置。
- 注入順序、backend 實際載入方式與保留邊界要有 rendered preview。順序是交付事實，不保證模型必然服從；權限仍靠 tool/runtime。

### 4.3 Effective contract 與 activation

沿用既有的 declared／effective／consumer／activation／evidence 五維，但擴到 instructions、workflow entry、skills 與 tools。最小診斷應能回答：

- source realpath 或 package identity、內容 digest、contract version；
- user 宣告了什麼、daemon render/stage 了什麼；
- 哪個 instance session 取得哪個版本；
- 變更需要 next read、rescan、resume restart 還是 fresh restart；
- 是否可確認 backend 已載入；不能確認則顯示 unknown，不以「已寫檔」充當「已讀取／已遵從」。

L1 的 spawn-time、symlink 的磁碟即時可見、filtered stage／copy 的更新、backend 的 skill discovery cache 是不同層，不再用一句「即時生效」蓋過。Resume 能否重載 system instructions 需 per-backend evidence；若某 backend 不支援，不把 resume 按鈕顯示為已套用。

v1 使用現有 configuration／diagnostic API 增補資訊，無需建立另一個 authority service。熱路徑只提供短摘要與 locator；完整 manifest 放診斷面，不把長 hash、build metadata 與所有 peer 名單重複注入每一輪。

### 4.4 Skill 供應沿用現有系統

官方已有 unified skill source、allowlist、stage、symlink／copy 與 install/update 機制 [E]；不另建 marketplace 或 package manager。

新增／修正的契約是：

1. 安裝來源、版本及 user-owned／daemon-managed ownership 可查；同名衝突不得靜默覆蓋自訂 skill。
2. 官方 workflow 必需的 skill／entry 與 optional skill 分開。required 缺失、allowlist 排除或 target 不可讀時，該 workflow 不標 ready；optional 缺失可 warning。
3. native skill discovery 與檔案可讀分開。沒有 native skills 的 backend，只提供已驗證的短入口／file-read fallback；連必要文件都不能讀時，不宣稱支援此 workflow。
4. references 以 package root／loader 提供的 source identity 定位，不依 product cwd 猜同名檔；遷移後可解析，不留下固定 `/Users/cheerc/...`。
5. stage 更新與 GC 不使活躍 session 的必要入口消失；內容更新、allowlist 更新、copy refresh 均有清楚 activation。
6. 禁止 daemon 自動重寫使用者的 custom role／skill 以追新預設。升級先呈現差異與相容性，再由 user 選擇。

### 4.5 Protocol 瘦身有具體交付

把現有內容逐段分類成：runtime invariant、單工具 contract、跨工具 workflow、可選方法、歷史理由。

- Kernel 只保留 bootstrap 必需短契約與 pointers。
- MCP description 保留 action-local 精確語義，不搬整本 protocol。
- 官方 workflow 按 intake／implementation／review／closure／recovery lazy-load；actor 只讀當前階段需要的部分。
- project facts、角色個性、incident history 不進官方 always-on 區塊。
- 移走內容前驗 discoverability、forcing、fail-stop；不能用「daemon 會擋」刪掉尚未機械執行的唯一行為邊界。

**完成判準**不是總行數下降：沒有 memory 的 agent 從 spawn surface 能找到唯一入口、完成情境，不需 General 臨場補充。token 成本量測採真實 rendered surface 與實際讀取，不拿 `tools.rs` 檔案大小當 MCP token 成本。

## 5. Dispatch、review、completion：把容易用錯的接縫變成產品契約 [P]

### 5.1 先承認目前是多個狀態，不用一個 success 假裝完成

| 面向 | 要回答的問題 | 不得推論為 |
|---|---|---|
| task identity | 工作是否存在、誰負責、在哪個 board | 已通知或已開始 |
| dispatch delivery | 拒收／持久排隊／已投遞／已被處理 | assignee 已完成 |
| execution | scope 的產物是否 ready，在哪個 exact subject | 已 merge／parent 已 done |
| review | assignment 有效嗎、receipt 是否已接納、審的是哪個 head | 測試必然成功、文字 claims 必然正確 |
| integration | PR 是否真的 merged，landed identity 是什麼 | 所有 cleanup／部署／user acceptance 完成 |
| closure | 哪個 task terminal、誰結算、依據是什麼 | inbox、CI handoff 也必然清掉 |

先對既有資料增加一致的回應與查詢投影，不為顯示方便重寫 task status enum。`ready` 只作明示的交付里程碑，不能再叫 terminal report 卻同時等待 merge。

### 5.2 Dispatch 的公共前置與結果

保留 `task.create` 建 identity、`send task` 派工的基礎 primitive，但官方 normal workflow 必須閉合兩步，讓「只 create 忘了 send」可被看見。

Dispatch admission 必須驗證 task／assignee／project／repository、communication reachability 與後續 claim/report/closure 能力。A-1 的開放通訊不代表跨 board mutation、review assignment 或 merge authority 同時開放。

對 PR-producing dispatch：

- source repo、base ref／base OID、target branch／expected head 的意思明確；base 是分支起點，expected head 是 exact postcondition，不能混用。
- 成功回應提供 actual repository／branch／HEAD／binding 與 watch identity；錯誤回應說明已提交哪些副作用。
- 不匹配不得先把工作交給 agent 才靠 L1 要他們發現；至少 admission fail 或明示 partial state 與 recovery owner。
- 一個已持久接手的 busy-park／長工有可查的 operation／dispatch identity、queued/running/completed/failed 狀態與下一步；不讓 caller 把 timeout 當成未執行而重送。
- 重送以同一 request／operation identity 查詢或去重；不是把同樣的自由文字內容當可靠 idempotency key。

v1 可先強化現有 `send`，不要求一口氣新增萬能 `dispatch` API。是否合併 create+send 由後續 spec 根據 transaction 路徑決定；本版不改成 create 自動通知，避免破壞既有 caller。

### 5.3 分開 parent、execution、review 的閉環

官方 normal flow 以 branchless coordination parent、PR-producing execution task、reviewer-owned review task 表達責任，不讓 parent 因帶了 branch 意外被當 execution task。

- PR ready report 是 progress／交付，不在 merge 前聲稱 execution 已 terminal；如果產品另有真正 execution-complete 的語義，必須與 task done 明確分離。
- merge 由 daemon 或外部人完成均可；以 exact forge observation 與有效 linkage 收斂 execution，不依賴已 release 的 binding 猜 owner。
- parent 由其 closure owner 根據 acceptance 關閉；保留 P1 的治理代收與 done actor 可追溯，不要求所有使用者都採我們的「僅 assignee 自收」加嚴。
- review task 的完成、assignment 退休、worktree release 是可區分結果。正常完成後的 retirement 不應讓 reviewer 誤認遭抽單重送 verdict。
- cancel／supersede 必須處理對應 dispatch tracker、pending handoff、watch 與 assignment 的適用義務，或留下具名保留原因；不讓 terminal task 繼續報「未完成派工」。不因取消一張 task 而關掉別人的 shared watch。

預設恢復先查 exact object state。不得把「再送一次 terminal report」「再 task done 一次」「force」寫成通用修復方法。

### 5.4 Review 與 evidence 的公共契約

- PR identity、full HEAD、review class、reviewer identity／slot、assignment generation 必須由 typed authority 連結；free-text VERIFIED 不是 receipt。
- acceptance／source／scope／freshness 的最小 handoff shape 由官方 workflow 說清楚；不要求每種角色重複七個易漏欄位。
- handler 確實驗證公開 schema 承諾的 enum、必填與 action applicability；`branch` omitted、null、empty 的行為明列。未知／不適用 write key 不可默默 success。
- receipt accepted、report delivered、public mirror success、review task closed 分別回報，允許查詢／補做缺的一步而不重造整份 review。
- `evidence_digest` 若只驗格式就必須如此宣告；若要驗 byte 一致性，需有 frozen evidence bytes／manifest 可重算。**hash 與 presence check 都不證明論述正確**。
- HEAD 改變使舊 subject evidence 不能批准新 subject；base drift 按有效 gate 處理。不能用重新 stamp 掩蓋實際審查範圍改變。
- review 方法、風險判斷與「scope fidelity 是否符合需求」仍由人／agent 負責。daemon 不當語義裁判。

### 5.5 錯誤必須能帶領恢復

針對本次會改的公開 action，回應至少能表達：stable code、subject、執行／副作用狀態、目前缺什麼、下一步由誰做、是否可重試、以及必要的 fresh identity。沒有合法下一步時，明說需 operator，而不是提供一定撞同一道 ACL 的建議。

若 status 受 caller scope 限制，empty 必須區分「本 caller 不可見」與「不存在」能區分的部分；不能拿投影落空當不存在證明。MCP 描述、CLI help、GUI action form 使用同一 action contract 資料，不各養欄位清單。

## 6. no-CI：CI 條件可宣告放行，工作流仍有真相 [P，承接 A-2]

### 6.1 狀態模型

no-CI 不等於 no-review、no-tests、force merge、offline forge 或所有錯誤都忽略。

至少分開：

- **observed CI**：provider 對 exact subject 的實際結果，含 pending／failed／passed／absent／unknown。
- **CI policy**：required 或 declared opt-out，以及 policy revision、範圍、理由／來源與生效時間。
- **effective CI eligibility**：此時對這個 subject 是否滿足 CI 條件，basis 為 observed 或 declared。

具體 enum／欄位由 spec 決定；**declared eligibility 必須存於 `CiState` 之外的 policy store，並在 HEAD 變更路徑明確保留；**`merge_readiness` 維持只吃 `PrState`，declared eligibility 以何種方式進入該函式由 spec 定義，本檔不預設**；**每個 HEAD 各自的宣告紀錄**在現況 `pr_state_filename`（per-branch、非 per-head）下無處存放，儲存位置由 spec 定義**（現況 `CiState` 只有 `Pending|Green|Failed`，寫入為完整賦值，無 declared 分支）。不得把宣告塞成普通 `Green` 覆寫失敗紀錄。依 V6-1，宣告 mode 使 CI 條件對**所有** observed 結果通過——absent、pending、quota-like，以及 current HEAD 上真實執行後的 failed——不區分「是否真的執行過」，不建 provider-specific 分類器。observed 結果照樣保存與顯示；宣告事件與 merge 回應須**逐一列出當下 failed 的 checks**，讓「額度恢復或改回導入 CI 後忘了關宣告」能被看見。其餘 merge deficits（Draft、required review、exact head／base 等）保持有效。

### 6.2 範圍與切換

1. fleet.yaml 的 project policy 必須解析到 canonical repository identity，不只比對易漂移的顯示名或本地路徑。V6-1 的 operator 原話以「此 team」為選擇單位；宣告掛在 team 還是 project 由 spec 決定，但解析結果必須落到 canonical repo，並遵守下一條的衝突規則。
2. 多 team 指向同一 repo 時共用同一有效 policy；相反宣告 fail-closed 並指出衝突來源，不採 last-writer-wins。**衝突期間該 repo 的 CI eligibility 為 `unknown`（不默認 required 或 no-CI），直到衝突解除或 operator 指定優先。**
3. required → no-CI：在一個可辨識 policy revision 生效後重算 eligible subjects，持久記錄宣告 basis，再投遞事件；不得等一個永遠不會來的 provider run。
4. no-CI → required：宣告 eligibility 立即失效；只用 current HEAD 的 provider observation 重算。已排隊的 ready／handoff 事件在 action 前重驗；不拿先前宣告綠燈繼續 merge。
5. HEAD 前進：新 generation 仍可受持續有效的 project no-CI policy 涵蓋，但產生自己的 eligibility／宣告紀錄，不能沿用舊 HEAD evidence。
6. daemon restart：重建相同有效 policy 與未結義務，不把宣告狀態恢復成普通 CI pass。
7. ⚠️ **merge 路徑的 CI truth 寫入**：現況非 force merge 前的 `refresh_ci_truth` 會無條件 `record_ci_result(Green, sha=head)`；宣告期間必須禁止此寫入，否則撤銷宣告時 observed 的 failed 已被抹掉，直接違反第 4 條。
8. policy 來源缺失、repo 映射不明或 revision 無法確認：保持 unknown，不能默認 no-CI。

A-2 不要求阻止 agent 手改 fleet.yaml；可記錄的 actor 就記，無法可靠取得時記來源／digest／時間及 actor unknown，不偽造 operator 背書。**宣告寫入須記 actor／digest／時間；actor 為 agent 時 merge 回應的 basis 欄位標示 declared-by-agent**，否則宣告與 force 在 §8.1 威脅模型下等價（A-2 不被推翻，只是把取捨寫明）。

### 6.3 五條 consumer 同步，另補 post-merge

必須同時覆蓋 merge gate、`pr-ready-for-merge`、`ci-ready-for-action`、review-class-unresolved 診斷、**非 force merge 前的 CI truth 寫入（§6.2 第 7 條）**，而不只改第一條 if。post-merge arm watch 的分支／caller 也要走同一 policy 解析（現況 arm 時寫死 branch 與空 caller），避免「merge 前用宣告、merge 後用 observed」。

- 每個 readiness／handoff／merge 結果都帶 subject 與 observed/declared basis。
- 通知語意是「CI requirement satisfied by declaration」，不得顯示「測試通過」。既有事件名稱若為相容性保留，payload 與官方 handler 必須分辨 basis。
- `next_after_ci` continuation 能在宣告期間前進；需要真實 runtime evidence 的 reviewer 不因這個事件自動給 VERIFIED。
- CI policy 切換不改 review assignment generation、不撤回同 HEAD 的有效 source review；在 review 當時記 evidence context，在 merge 當時再記本次 basis，不事後改寫舊 report。
- post-merge exact-head gate 也需同一語義。專案若只要求 CI eligibility，可用明示的 declared outcome 收斂；若 acceptance 要求實測 runtime evidence，no-CI 不豁免它，需本機／其他 runner 的指定 evidence 或有權者另行修改 acceptance。

外部 forge 的 branch protection／merge 權限仍可能阻擋；daemon 宣告不會讓 GitHub 額度恢復，也不會繞過遠端 required checks。回應必須把 daemon-ready 與 forge-blocked 分開。

### 6.4 驗收

在隔離 repo 測 required／no-CI 往返、既有 failed run（含 current HEAD 真實執行失敗：依 V6-1 放行，且事件／merge 回應列出該 failed check）、完全無 checks、「quota-like pending」（測試情境名，非分類器）、HEAD 前進、daemon restart、通知重投、policy 衝突與 stale queued handoff。確認所有 consumer 使用同一 basis、宣告可追查，且 Draft、required review、exact head/base acquisition 仍然擋錯誤案例。CI 額度不足的情境不依賴先跑一個小 GitHub job 才能驗。

## 7. 訊息與監看：可交接的義務，不是靠多寫一句提醒 [P]

### 7.1 每種訊息有清楚 postcondition

官方契約區分 task/query 的處理義務、report/update 的資訊性質、channel reply 義務、CI handoff episode。收到訊息、drain、處理、reply／report、ack、task terminal 是不同事件。

Kernel 提供入口，完整 action mechanics 在工具描述，跨工具 sequence 在官方 workflow。不能讓人為了結一個 inbox row 必須先讀 private JSONL 或 downstream quirks。

新／改動的通知提供 typed subject、義務類型、有效 generation、settlement action 與 recovery owner；自然語言只是呈現，不能成為唯一機械判準。

### 7.2 事件驅動與持久 continuation

- work／watch 有唯一有效 continuation owner；dispatcher、CI subscriber 與下一個要動手的人不必是同一個。
- notification delivery 失敗、consumer restart、取消／改派時，義務可查並按既有 outbox／watch 機制重送或終止，不另做通用 message broker。
- redelivery 以 message／assignment／episode identity 去重；exactly-once 的承諾限於可證明的 state effect，不能聲稱網路通知絕不重複。
- accepted-in-progress 不要求 agent 定時盲猜或重新 mutation。長工在 deadline／terminal event／明確錯誤時續接，必要時可作一次具體查詢。
- 不強迫所有跨 team 訊息經 General；A-1 保留直接聯絡。使用者仍可在自己的 L1 採「先回 General」作組織政策，但那不是產品預設 ACL。

### 7.3 通道與 backend 差異

現有 live descriptions 已區分 delivering reply 與僅 transport acknowledgement [E]。產品應讓不同 adapter 共用同一語義與回應形狀，避免同名 reply 讓 agent 誤以為已回到 operator。

這次不改 Telegram transport 架構；只要求 official contract 與整合測試涵蓋「是否真的對外送達／是否只是 ack」。沒有該 channel 的輸入，不憑上一輪 channel binding 發到別處。

## 8. 治理與 identity：可救援，但不冒充安全隔離 [P]

### 8.1 威脅模型

單一使用者、same-UID 的協作環境。agent 可讀寫同 owner 的檔案，不宣稱 hostile-seat isolation。保留 cooperative authority 與資料正確性守衛的理由是避免誤用、可歸因與可恢復；不是把所有工具都叫 security boundary，也不是因非安全隔離就取消所有 guard。

P2 的順序：先整理 operator CLI、managed bridge、external client 的入口分類，再拒絕 Agent transport 下 anonymous／未知 managed identity 的全開 fallback。`operator` 字串不是 operator transport；未知姓名不是自動 external 身分。

### 8.2 Actor 分類

- managed instance：可解析到既有 instance identity 與 session；同 UID 可偽造的剩餘風險需明示。⚠️ **TUI（`src/app/rpc.rs:731`）經 `api::call_at` 走 loopback socket，並送空 `instance`（`:735`），與 bridge 同樣落進 `role_kind_for_instance` 的空值全開分支**（`mcp_proxy.rs:437-438`）；`Sender::from_env`（`src/mcp/handlers/mod.rs:214-215`）在 env 命中時會**改寫** `instance_name`、覆蓋 payload，但 TUI 行程不設 `AGEND_INSTANCE_NAME`（`src/app/` 0 命中），故此路徑**不存在** payload 覆蓋，問題是 anonymous fallback。P2 的 0c 必須覆蓋它。
- operator：使用既有可信 transport 分類，不靠 payload 自稱。
- external client：A-9 的自報名是 display/audit label；有明確 external surface，不能藉 `external-agent:` 前綴取得 managed／governance 能力。
- system action：記 daemon 的 execution actor，同時保留 initiating caller／operation provenance；不能只留下 `system:task_sweep` 抹掉誰要求。

### 8.3 權限合成與可理解性

沿用 per-(tool, action) grant 方向（依 V6-2 屬 v1 之後；v1 不新增 grants 模型）；`role_kind` 存在時 grants 與該 role 上限取交集。沒有 typed role 的**已識別** caller 使用其 explicit/default policy，不與匿名 caller 全開混同。

公開診斷須分開 advertised、role/grant allowed、handler/resource allowed、當前 mode restriction；被工具列表列出不代表可操作每個 target。operator／有權治理者可透過 supported writer 修改授權；不改動 A-8 的 decision 邊界。

### 8.4 治理救援與日常 commander 分開

| 能力 | 目標 |
|---|---|
| commander 的 creator-scoped task 管理 | 對自己建立且 scope 合法的工作回到正常 loop，不需全域 superuser |
| 治理者 task update／done | 跨 owner 協助，P1 保留 assignee，記實際 done actor、reason 與來源 |
| sweep／board 操作 | A-6 的可設定名單在入口層驗 initiating caller；dry-run／確認／fresh-state 檢查不刪 |
| plan governance | A-7 保留原 create/plan authority，不隨救援權限傳遞 |
| review-class correction | exact-generation 的既有受控 correction，**授權對象為「具權者」（V6-6，operator 2026-10-04 裁決；現況 `correct_review_class` 為 operator-only，本設計放寬，但「具權者」的具體資格由 spec 依 §8.3 的 grant／role 上限定義，不另造獨立授權面**）；不改成批次 metadata 改字串 |

治理者代收可以是有理由的行政結案，但不能生成不存在的 merge receipt／VERIFIED／實測 CI。正常完成與管理性終止要可區分；現況的 completion guard 只保護 assignee，不可被文案偽裝成全員 hard gate。

原 owner 離線／被刪時，留下可用的 recovery owner／operator 路徑；不能給一個根本無法登入的人作唯一下一步。commander 的完成通知回到原 requester，task done 不等於 commander 已批准下一輪 scope。

## 9. Decision board：連結、封存、治理 authority 不混成一件事 [P]

沿用既有的最小 refs：`caused-by`、`related`。task target 帶 board identity，issue／PR 帶 canonical repo，decision 用穩定 ID。note 是人讀文字，不是索引或權威。

- refs 描述關聯，不觸發 supersession、task completion、review authority 或 cascade close。
- `related` 可單向存放、查詢時雙向呈現；UI 去重並標方向，不新增跨 decision 雙寫鎖。
- 保留 typed supersedes 的獨立不可變／防環／ACL 契約。
- v1 可先掃描反向 refs；是否建索引以實測 latency／資料量決定，不沿用沒有量測依據的「3000 筆」門檻。
- ACL 依 A-8，治理者不能藉 refs／close 繞過既有 decision authority。answer 的既有非作者可答行為保留為 known limitation，不在本次假裝修掉。

**Close**：沿用既有 `update archive:true`；如果加 alias，只是同一 contract 的入口，不增加第二個 writer。預設 list 不顯示 archived，但歷史查詢／refs 仍能解釋「已封存」，不把它當不存在。

**Reopen**：這是本提案的設計延伸，不是 A-5 的原話。本提案建議保留於此工作包，但只重開未 superseded 的 leaf；仍走原 decision ACL 並記 audit。已有 successor 的決策不能靠 reopen 恢復治理 authority；需要新 decision／既有 supersession 路線。重開不回溯修改已建立 task 的 review_class，不重播舊 question 的 timeout 或通知義務。

**跨版本修正**：給新 `Decision` 加 unknown-field preservation，不能改變**已發布舊 binary**丟未知欄位的行為。需要先建立 reader/writer 相容性表及可回滾版本下限：compatibility reader/writer 先部署，再啟用 refs；不相容舊版不得被當作安全 writer。驗證 `new write → 指定舊版 read/update → new read`，只承諾實測通過的版本組合。不能宣稱 `serde(flatten)` 一加，所有過去版本便自動安全。

## 10. 恢復、worktree 與跨 backend：把我們的補救手冊縮回可選內容 [P]

### 10.1 恢復的產品部分與自訂部分

Daemon 負責 live task／inbox／binding／watch obligations 與 resume admission。官方 recovery workflow 負責查明仍有效的工作、避免重跑已完成副作用、將 unavailable truth 明確交回 owner。自訂 handoff 保留 human delta、Required Reading、未決想法；不複製 live board 成另一套真相。

- session continuation 的持久位置用 instance workspace identity，不是當前 cwd 或會被釋放的 worktree。
- handoff save 不等於 restart 授權；fresh restart 前需要真正 readiness evidence，pane 看起來安靜不足。
- restart 操作結果以 daemon operation／successor identity 為準，不靠「tool 有回應所以失敗／沒回應所以成功」這類 backend-specific 推論。
- `[AGEND-RESUME]` 是恢復自己的義務，不是新 operator 工作命令；`[AGEND-AUTO]` 只續既有工作。
- 手動 `/clear` 若 backend 沒提供一致 trigger，就顯示需手動恢復；不宣稱 daemon 能偵測所有 context reset。
- daemon restart 可能中斷 managed process；process continuity、conversation resume、worktree preservation、durable state 四者分開呈現。

v1 提供官方共用 recovery entry，使用者可以換 human-delta 格式；不要求大家搬用我們完整 `handoff` 的每段散文、save 時機或所有個人回顧流程。

### 10.2 Worktree 的產品契約

- binding／source repo／actual HEAD 可直接查，release／rebind 的結果有具體 postcondition，不讓 agent 只看 `released:true` 猜目錄已刪。
- readonly source inspection 不應被 silently redirected 到別的 bound repo 且不提示。官方必須提供無歧義的唯讀查證路徑或明確 expected-repository guard；不是讓每個使用者學會一套 bypass。
- canonical checkout 的 ff sync 不是 merge 已落地的證明，也不應偷偷切 operator 的 branch。v1 若仍由 operator 做，官方文件明列 default-branch／clean／upstream／ff-only 條件。**本 fork 的部署 branch 唯一為 `main`**（V6-9）：ff sync 只對 main 做，不對任何部署分支做。
- release-before-merge 是我們目前的 local tightening，不上收成所有專案不變量。產品應支援合法 release timing 及 merge 後 cleanup，守 dirty work／identity／shared repo 邊界。
- bypass 的推薦形式必須有明確 scope、audit 與副作用；保留既有的限時／alias 收斂方向，但不宣稱六個環境變數已被證明是事故根因，也不把限時當作撤銷能力。

### 10.3 Backend capability 而非品牌推測

官方已有 capability matrix [E]，v1 在既有 adapter／diagnostics 上補齊 workflow-critical 能力：instruction delivery／reload、native skill discovery、可讀檔範圍、resume／fresh、MCP／CLI transport、可靠的 busy/idle 信號、context／usage-limit observation。

矩陣記 backend CLI 版本、provider/proxy override 與證據。process alive、screen idle、model 正在思考、task done 不是同一狀態。unknown 不可填 healthy，更不能用 unknown 自動 force restart。

不要求首版每個 backend 同時達到最高能力；以實測支援範圍發布。unsupported workflow 有明確拒絕或文件化 fallback，不以品牌名保證所有版本行為一致。

## 11. GUI 與 external client：共用 contract 的 consumer [P]

### 11.1 GUI

維持 A-10，同 repo 獨立發布；tmux 是工作環境，GUI 是管理面。v1 先做：

1. task／decision／refs 的可追溯白板與 project 進度；
2. 既有 daemon writer 支援的 model／effort；team 與 display metadata 的 target 能力不足，但**兩者屬 v1 之後**（W7「後續」列），先補 daemon 端點時不得讓 GUI 直接改 raw state；
3. declared／effective／activation／observed evidence，含 no-CI 的宣告標記（**activation 維度在 W2 落地前為 unknown**，見 W7 v1 切面）；
4. daemon restart 前置提示、操作進度、部分失敗與 recovery owner。

第一個 slice 維持「讀白板 → 看單筆 task → 改 model → 看待套用 → 按 backend 支援的方式套用並驗證」。不得把 `takes effect next respawn` 當作已確認 effective model。

通用 YAML 編輯器仍延後。GUI 不自行複刻欄位白名單、YAML 註解搬移、lock 或 policy evaluator；CLI／MCP／GUI 應共用 daemon writer 與有效設定資訊。既有 writer 是否完整保留註解位置另立相容性測試，不因使用 tool 就宣稱無損。

GUI 的可點動作來自 capability／action contract；daemon 在執行時重新驗證，不把 UI disabled 當授權防線。draft 留存與衝突回應不悄悄覆蓋 operator 的外部手改。

### 11.2 External client

保留原生 Desktop／CLI 與 GitHub 合作方式，不做外部專用 agent runtime、daemon 主動推播或偽裝成 managed backend。A-9 名稱必填，read-only surface 以 action 級 grants 表達。

外部 client 可送訊息給已登錄 target；離線 delivery 是排隊不是已處理。後續 PR 討論可走 forge，不承諾 external client 能接 fleet inbox。

External implementation／review 的 claims 不直接變成 daemon receipt。內部 coordinator 用明確 external author identity／exact subject 接入既有工作流；保留 review-author 獨立性，不靠自報名稱取得任意 task／治理權限。無需為此改 task schema 加 `external` flag。

## 12. Fork、部署與回復：現在服務 cheerc 的產品 [P]

### 12.1 Repository 與更新策略

產品首頁、**issue intake**、release/update source 及預設 forge target 一律指向本 repo；**部署用 binary 只從本 repo 的 `main` build**（V6-9）。原 upstream 保留 attribution、license／NOTICE 與可選參考 remote。歷史 issue 連結不機械替換成同號 fork issue。

不以前輪「fork 落後 1641 commits」的 ref 快照決定現在的 sync：該數字已過期，running/source baseline 已是另一個 identity。W0 查清本 repo 的 default branch、source checkout、running build、remote refs、工作中 branches；只採明確選定的 base，不重寫已發布的 default history。**部署 branch 唯一為 `main`**（V6-9），不再有 current-build 這類部署分支。

不預設持續自動 sync suzuke；後續安全修正／選取更新是維護政策。保留外部 upstream 不是把產品方向繼續交回 upstream。

**Upstream issue 接收**（operator 2026-10-04 要求不得遺漏）：**新的 issue 一律開在本 repo**（V6-8）——本段只處理**上游既有的 open issues**：它們可能就是待改的項目，必須逐一處置，不能因換 repo 而漏掉。

- fork 的 Issues 已由 operator 開啟（2026-10-04 查證 `hasIssuesEnabled: true`）。
- GitHub 的 issue transfer 只限同一 owner，無法從 suzuke 轉到 cheerc（依 GitHub 規則判斷，未實測）；收進來的 issue 在 fork **重開並連回原 issue**，不機械複製編號。
- 每個 upstream open issue 歸入四類之一：**併入某工作包**／**獨立 bug 修正**／**不適用於 fork**／**延後**，並記錄理由。作者是 cheerc 或 suzuke 都同樣處置。
- 分類以 W0 執行當下重新查得的清單為準；附錄 A 只是 2026-10-04 的初判快照，不是權威清單。
- suzuke 之後新開的 issue 不自動同步，比照上段「選取更新」的維護政策處理。
- fork 內重開 issue 不要求在 suzuke 留言或關閉；對 upstream repo 的任何留言、關閉都屬對外動作，由 operator 個別決定。

### 12.2 平行環境

`AGEND_HOME` 不等於完整隔離。測試需獨立 source clones、worktree/git common-dir、binary paths、bridge/shim PATH、run/token、channel endpoint 與 session storage；關閉真實 Telegram／外部寫入或使用隔離 endpoint。

這也是「用現行 daemon 打造新 daemon」的保護：開發 fleet 不因測試安裝覆寫自己正在執行的 binary；測試 home 不得對正式 repo 或正式 task board 寫入。

### 12.3 升級契約與 rollback

- release manifest 記 daemon／bridge／shim 等需配套 binary、contract／state schema、支援 backend CLI 與 official workflow package 版本。
- bridge／daemon 能力不相容時顯示 mismatch；v1 尚無 negotiation 的路徑維持成套替換，不冒充 mixed-version 安全。
- quiesce → 一致快照 → 記 binary/state/schema identity → canary → 對帳外部副作用。不能逐檔 copy 活躍多 store 後宣稱一致。
- rollback 前先確認 old reader/writer 相容性、backup 後已完成的 merge／訊息／deployment；不能單純還原磁碟把已發生的外部世界「倒回去」。
- release note 列出行為契約 delta、breaking/deprecated capability 與 downstream removal candidates，不要求每個使用者讀數千 commits 才知道 SOP 是否失效。

## 13. 工作包與順序 [P]

這是 proposal 級 dependency，不是已授權派工或 implementation plan。每包要再定 exact base、scope、tests 與 PR boundary；不是要求整份一次大改。

**v1 切線原則（V6-4）**：v1 是「現有 customization 仍安裝著、可用來開發 cheerc fork 的第一版」，不需要取代 downstream；官方 workflow 尚未涵蓋的部分由現行 L1／skills 繼續負責。每包只交付「v1 最小切面」欄那一刀，「交付」欄其餘內容為後續版本。完成門檻以 §14 scenario ID 表達，不用開放式描述。各包切面由 General 提出、at-team-lead 審查同意，屬 [P]；標 V6-x 者為 operator 裁決。

| 工作包 | 交付（全貌） | v1 最小切面（其餘為後續） | 前置 | v1 完成門檻 |
|---|---|---|---|---|
| **W0：fork 基線與隔離** | canonical fork identity、獨立測試環境、load-bearing 事實重驗與 release/rollback 基線 | 確認 owning team、source_repo 與 baseline；隔離 `AGEND_HOME`＋獨立 source clone＋關閉正式 channel；選定 base；只重驗 v1 各包改動所依賴的 source 事實；**已演練、可退回前一 daemon 版本並繼續工作的 rollback 程序**（V6-4，手冊級即可）；**upstream open issue 全數分類**（§12.1）；**確認 repo 解析指向 `cheerc/agend-terminal`**（§13 執行前置）；**選定 base 並證明該 base 含本文引用清單中的全部 source 錨點**。後續：release manifest 自動化、手冊以外的 rollback 工具 | 本提案接受、owning team/repo baseline 確認 | 不碰正式 state；**base 涵蓋本文引用的每個機制**（逐項以 symbol／doc-comment 定位，**不以行號定位**；本文的 source 行號綁 `184a5fe`，選定其他 base 須以 symbol 重新定位後才可引用本文的 source 事實）；未查證項有 owner；當下 upstream open issue 每一個都有分類與理由、零未處置；T16 |
| **W-V：驗證工具鏈可信度** | 測試 fixture 的隔離與生命週期、必要工具缺失時的 fail-loud 行為、同一 commit 在不同環境產出相同結論的可重現性 | 先做「讓髒環境不再改變結論」：fixture 不建在 `$HOME` 且失敗即清理（RAII）；fixture 依賴的 repo 狀態（`origin`／`HEAD`）由測試自建而非假設繼承；必要工具缺失時 fail-fast 或明確標記 bulk 結果不可信，不以 warn-and-fallback 產出假綠。**後續**：把上述健全性檢查收斂成一組可重複執行的 self-check，並在 §14 驗收前先跑 | W0（需既有 base 可驗證） | T17 |
| **W1：caller 與 authority** | P2 operator/managed/external 入口遷移、anonymous fail-closed、action applicability 基礎 | 完整 P2：0a→0b→0c 子順序，含「未知 instance 名」同樣 fail-closed，**並重新確認 `orphan_reconcile` 的 actor 檢查與 0a 不脫節**。後續：action applicability、unknown write key 拒收 | W0 | T13；且至少涵蓋早期分析的三個案例（空值／不在 fleet.yaml 的未知名／真實具名 instance；不可用 `general` 當正向案例）與早期分析記錄的**三個**已知缺陷（驗收可能在未修時通過；修法不可擋住 operator 自己的 CLI；**改動 actor 來源會讓 `orphan_reconcile` 的 `actor != "operator"` 字串比對脫節**——W1 v1 的 0a 正是此變更，驗收須含：(a) agent transport 送 `params.instance="operator"` → 被 `trusted_operator` 擋；(b) operator CLI 走 0a 新路徑（送保留字）→ `orphan_reconcile_preview/apply` **必須仍可通過**，證明 0a 沒把 operator 自己關在門外）。**誠實邊界：此驗收只測得到無心之失，測不到 agent 自設 `AGEND_INSTANCE_NAME` 的 env 偽造** |
| **W2：instruction／skill 契約** | 小 kernel、official workflow phase package、custom source ownership、effective manifest 與 activation | **V6-3：v1 切面待 spec 階段單項討論後決定**；本檔不預定 | W0；authority query 整合依 W1 | **v1 不適用（V6-4）**；由 spec 定義 |
| **W3：no-CI 閉環** | §6 policy、全部 consumer、宣告事件與 post-merge 行為 | 核心閉環必須一次做完（只做一半會重現「靜默健康」），但**下列三項介面由 spec 定義，W3 不得在三者未定前宣稱閉環完成**：(a) declared eligibility 進入 `merge_readiness` 的介面契約；(b) per-generation 宣告紀錄的儲存位置；(c) post-merge 觀察對象的 policy key（現況 arm 寫死 `branch:"main"`，與 feature-branch 的 policy key 不同）。⚠️ 另註：`ci watch` 空 caller 現況被視為已授權（`ci/watch.rs:97-99`），屬 P2／W1 範圍：開關解析到 canonical repo、衝突 fail-closed；不記成一般 Green 的獨立宣告狀態；五個 consumer（含 §6.2 第 7 條的 CI truth 寫入）、post-merge arm 的 policy 解析、宣告事件與 merge 回應的 basis 一起改；宣告涵蓋含 failed 的一切 CI deficit 並列出 failed checks（V6-1）；HEAD 前進產生新 generation；關閉宣告後在 action 當下重驗；post-merge 接受宣告結果。後續：receipt 層的 evidence context 標註、已排隊事件的追溯清理 | W0；policy identity 凍結 | T04、T05 |
| **W4：dispatch／closure／recovery** | §5、§7、§10 的 operation state、身份對帳、義務收斂；commander 回報路徑 | 只為本版動到的 action 補 §5.5 結構化錯誤（merge readiness、branch dispatch 拒收、terminal report 的 `closed:false`）；tool 回應更正 PR-ready 語義，官方說明文字的落點依 V6-3 spec 結果；cancel／supersede 結清 dispatch tracker（否則 30 分鐘後誤報）；exact **HEAD** 驗證引用現有 `send` 的 `expected_head`，不新增（`send` 現況無 `base`／`from_ref` 參數，`expected_head` 約束的是 target HEAD，不是 base 分支起點，見 §5.2「不能混用」）；**base 驗證屬後續版本**。後續：operation identity／idempotency、**dispatch 的 base 驗證（需新增參數，與「不要過度開發」取向的取捨由 operator 裁決）**、一般性 dispatch admission 可達性檢查、commander 自動回報（v1 沿用明確 `send`）、busy-park 查詢面 | W1；說明文字部分依 V6-3 spec | T06、T09（**v1 僅驗 source／HEAD；base 部分屬後續**）；cancel／supersede 情境（T08 中 dispatch tracker 部分） |
| **W5：治理與通訊** | P1／A-1／A-6／A-7、creator-scoped commander 權限、grants | P1 按 key 分類 update、A-6 入口層名單、A-1 放寬到 member、commander creator-scoped 權限、A-7 不變、review-class correction 授權「具權者」（V6-6，「具權者」集合由 spec 依 §8.3 定義）。**後續（V6-2）：per-(tool, action) grants，與 GUI 權限勾選一起做** | W1 | T10 的治理部分：治理可救援且 owner 不漂移；開放通訊不擴張 resource authority |
| **W6a：compatibility writer** | §9 的跨版本相容性 | compatibility reader/writer 先行、版本矩陣測試；**獨立 owner、獨立驗收** | W0 | new write → 指定舊版 read/update → new read 的指定版本組合通過 |
| **W6b：refs 啟用** | §9 的兩型 refs 與 traceable board | `caused-by`／`related` 兩型 refs、沿用既有 close（`update archive:true`）。後續：reopen（本提案的設計延伸，A-5 原話未提）、反向索引、close alias | W0；W6a | T14（reopen 部分除外） |
| **W7：GUI consumer** | §11 vertical slice，再整合 grants／no-CI／activation | 既有規定的 GUI 五步 slice（讀白板 → 看單筆 task → 改 model → 看待套用 → 套用並驗證）。後續：grants UI（V6-2）、team 編輯、display_name（需先解除 `set_metadata` 只能改自己的限制）、restart 管理 UI、refs 圖 | W0 起可做唯讀；寫入依各 writer 工作包；**五步第 4 步「看待套用」依 W2 的 activation 診斷，W2 若延後則該步降為「顯示 daemon 已寫入、activation 標 unknown」**（§11.1 不得把 `takes effect next respawn` 當作已確認 effective） | 無第二套 policy／YAML writer；五步 slice 走完且生效狀態與真實 consumer 對齊；**W2 延後時本門檻只驗「daemon 如實回報 activation unknown」，不驗五維呈現是否夠用，後者留待 W2 落地後重驗** |
| **W8：發行與 downstream 減法** | clean-install 指南、contract delta、官方 samples、canary 與 customization retirement | 只交付 release contract delta 與單 backend 的 T01 驗證（v1 階段容許現行 L1 在場）。downstream 退役整批屬 v1 之後，逐步進行（V6-4）；退役的終點里程碑是 V6-5（全部 instance 只靠 `role:` 吃 daemon 預設） | 對應功能包先通過；全版 release 依 §14 | T01（單 backend，**容許現行 L1 在場**） |

**v1 scenario 子集**：T01（單 backend，**容許現行 L1 在場**）、T04、T05、T06、T08（僅 cancel／supersede 的 dispatch tracker）、T09（**v1 僅驗 source／HEAD**）、T10（僅治理部分）、T13、T14（reopen 除外）、T16；其餘 §14.1 情境屬後續版本。

依檔案不同只能判斷可分工，不能直接宣稱可安全平行：W2/W3/W4 共同改 readiness、workflow 文案與回應語義，先凍結 interface，再由 owning Lead 排 PR。W7 不必等所有功能完成才開始，但不能用 mock 權限當成已可交付。

**執行前置**：operator 2026-10-04 已決定實作在 fork repo 進行，並將 fleet.yaml 中 `at-team` 的 `agend-gui-dashboard` 改為 agend-terminal——`at-team` 即 owning team，PR 送往 `cheerc/agend-terminal`。as-of 2026-10-04 live `team list` 已生效（project_id `agend-terminal`、source_repo `/Users/cheerc/agend-terminal`）；派實作前仍須讀該 source repo 的 `CLAUDE.md` baseline。

✅ **repo 解析已指向 cheerc（2026-10-04 處置完成）**：daemon 以 `git remote get-url origin` 解析 owner/repo（live `bind_self`／`repo checkout` 說明），auto-bind、ci watch、merge 因此必須落在 cheerc。operator 刪除舊 checkout 後重新從 `cheerc/agend-terminal` clone，最終狀態：

| 項 | 值 |
|---|---|
| `origin` | `https://github.com/cheerc/agend-terminal`（push 目標） |
| `upstream` | **已移除**（operator 2026-10-04：徹底不使用上游 repo）。讀取上游既有 issue 需顯式指定 repo（`gh -R <upstream>`）；**新的 issue 一律開在本 repo**（V6-8） |
| local `main` ＝ `origin/main` | `184a5feff9d530834e44c4324118e0966675494f`（fork 時以 `merge --ff-only` 對齊上游並 push；其後已合併本 fork 的兩支 PR） |
| gh 預設 repo | `cheerc/agend-terminal`（寫入該 checkout `.git/config` 的 `remote.origin.gh-resolved`；實測無 `-R` 的 `gh pr list`／`gh issue list` → 0 筆，`-R suzuke` → 5 PR／21 issue） |

✅ `git remote -v` 只列 `origin`（fetch 與 push 皆 cheerc，無 suzuke 條目）；`gh repo view cheerc/agend-terminal --json parent` → `null`。README 內的 suzuke 連結屬 attribution（§12.1 保留上游出處），非解析路徑。W0 須在設定／診斷面區分 nameWithOwner 與 parent（fork metadata 顯示 suzuke 的情形）。upstream issue 讀取一律用 `gh -R suzuke/agend-terminal`，不依賴 remote。✅ **已驗收（2026-10-04 W0-DOC-1 首次 dispatch）**：`binding_state` 的 首次 dispatch 的 CI watch 目標即本 repo 的該 feature branch；`ci action=status` 的 `repo` 欄位 = `cheerc/agend-terminal`（daemon in-process 解析，非僅 operator shell）。仍需在第一次 merge 後確認 merge receipt 與 post-merge watch 的 repo。

## 14. 驗收：用陌生使用者的閉環，不用我們自己的熟練度 [P]

### 14.1 有限的情境矩陣

| ID | 情境 | 可觀察的通過條件 |
|---|---|---|
| T01 | clean install、任意角色名字、無 downstream memory（**v1 期間不要求「無 memory」——現行 L1 仍在場；此條件於 V6-5 里程碑重驗**） | 可從公開入口完成最小 PR workflow，不需 General 補 recipe |
| T02 | 同 backend，不同 persona／skill 組合 | 角色差異保留，使用同一 task/review/closure 契約 |
| T03 | 兩種實際支援的 backend／一種受限 discovery | capabilities 顯示正確；有證據的 fallback 或明確不支援，非靜默忽略 |
| T04 | CI 完全無 run 或 quota-like pending | no-CI 宣告能推動全部 continuation；沒有假稱測試成功 |
| T05 | 宣告撤回、HEAD 前進、舊事件排隊 | stale readiness 不能批准 current merge；valid review 不被無故洗掉 |
| T06 | branchless coordinator／commander → execution → review | 不誤綁 parent；原 requester 收到結果；各 task 在正確條件結算 |
| T07 | busy park、side-effect timeout、回應遺失 | 查同 operation，不重複 mutation；失敗有 recovery owner |
| T08 | task cancel/supersede、review 正常退休 | tracker/handoff 收斂；無幽靈催辦、不誤關別人的 watch |
| T09 | source/base/HEAD 不匹配；typed review 過期 | 在正確入口拒絕，給可執行 remediation，不引導 force |
| T10 | 原 owner 不在，具權者救援 | owner 保留、actor 可追溯，無偽造 CI/review/merge evidence |
| T11 | 自訂 instructions、多 instance/shared workspace、missing skill | user bytes 保留，collision 明示，required workflow 不假 ready |
| T12 | L1／skill 更新、resume/fresh/restart | 分辨 staged／loaded／unknown；舊 session 不誤用新契約 |
| T13 | external 自報名、匿名 managed、operator CLI | 分類正確；自報 operator／external prefix 不取得 managed 特權 |
| T14 | refs 寫入、close/reopen、mixed-version writer | 不丟未知資料、不重活 superseded authority；只承諾測過的版本 |
| T15 | 外部人 merge、forge block、release timing | readiness/merged/closure 分離；不需存活 binding 猜歸屬 |
| T16 | isolated upgrade／rollback | source clones、state、binary、channel 不污染正式 fleet；外部副作用對帳 |
| T17 | 髒 `$HOME`／髒 worktree、必要工具缺失、fixture 缺少 repo 狀態、**以及測試根本未執行卻回報結論** | 同一 commit 在乾淨與髒環境產出相同結論；fixture 外洩為零（失敗亦清理）；工具缺失不產出 bulk 綠燈；**每一則失敗都能指認其為真迴歸或環境／工具成因，且「測試未執行」必須以非綠呈現**——非測試步驟（報告上傳、產物收集）失敗所產生的紅燈不得與真迴歸同形 |

不是所有情境對所有 backend 做笛卡兒積。每個發布宣稱的能力至少有對應 entry-point integration evidence；不支援的組合列清楚。runtime／concurrency／replay 涉及的變更須真實入口與故障注入，不能只測一個直接塞參數的 helper。

### 14.2 有用的量測

先固定 scenario 與分母，再量：完成一項工作需要的 tool calls、跨 source 查詢與 extra reads、人工救援次數、陷入不明狀態的時間、重複通知與 mutation、有效 prompt bytes/token、customization 中可安全退役的 semantic segments。

本機歷史量測曾以 event regex 命中 21.69% 描述「流程受阻事件」（口徑：kind 匹配 `den｜block｜refus｜violat｜force｜orphan｜fail｜reject｜stall｜stuck`，分母 `event-log.jsonl` 全量行數 47,217），**不是「正常 task 失敗率」**；force 事件也不單憑數量證明 ACL 因果。重新量測要有 task／dispatch correlation、時間窗、outcome、retention 範圍。某月份 force 歸零不能推出「使用者學會繞過」；那需要額外 evidence。

### 14.3 不做散文鎖定

機械測試驗 schema、enum、transition、identity、idempotency、authority 與 bytes preservation。自然語言檢查用 authority map、scenario replay 與獨立 review。不可把「裁決 ID 出現至少三次」「必須保留某一整句」當設計正確性；重複三份本身就是漂移來源。

在 release 前執行一次沒有舊 memory 的實際 bootstrap／workflow exercise；靜態 pointer 檢查只能證明文件可達，不能取代 actor 能完成工作的 evidence。

## 15. Downstream 減法與明確不做 [P]

### 15.1 上收候選與退役條件

| 現有 downstream 內容 | 上收目的地 | 何時可刪 |
|---|---|---|
| create/send 配對、binding 對帳、branchless parent | daemon admission + official dispatch workflow | T01/T06/T09 與契約已上線 |
| task/inbox/CI episode 結算差異、timeout 不重送 | 公開 result contract + recovery workflow | T07/T08；新 error 有可操作下一步 |
| typed review 欄位／slot／current-head／receipt 判讀 | review primitive + official review phase | T09/T15；不再依賴私有 JSON 查因 |
| L1/skill 生效、farm 可見性、missing entry | skills/instructions diagnostics + package contract | T03/T11/T12；每個受影響 actor 可用 |
| handoff 的 trigger/admission/live-state 去重 | daemon adapter + official recovery entry | T12；unsupported reset 已誠實標示 |
| 一般 daemon known-quirks、changelog 語義依賴 | 官方 docs／contract tests／release delta | 該版本事實重驗；不是把過時 manifest 原樣搬家 |
| shim readonly 查錯 repo／release 結果判讀 | 正式 tool/shim contract 與診斷 | 有對應 positive/negative 行為證據 |

**移植完成的判準（V6-5）**：第一個「downstream 已移植到 upstream」的穩定版本，必須做到我們所有 fleet.yaml instance 移除 `instructions:`、只留 `role:`，仍能以 daemon 預設完成各自角色的工作（以 T01／T02 在該版本重跑為證據）。在此之前每一批退役都要可退回前一 daemon 版本（V6-4）。V6-5 不等於把 §15.2 的組織偏好寫進 daemon 預設；達成前，spec 須為 §15.2 各項與 operator 常設授權（目前只存在 general.md、lead.md 等 L1）指定 `instructions:` 以外的落點（結構化 policy、project baseline、skill 或 decision），或由 operator 明確確認捨棄。

順序是 official additive capability → affected actor readiness → downstream retirement。正在執行舊 L1 的 session 由 owning authority 安排 cutover；不能只因 disk 更新就批次刪掉舊入口。每批用同一 contract delta 列出「取代哪個 workaround」與「仍保留什麼」，不另養一套永久重複的治理資料庫。

### 15.2 保留在使用者側

不把下列項目變成產品強制規則：

- General 必須是唯一跨 team 中介；
- 我們的 ac-team／sub-* 名稱、兩個 skill farms、絕對路徑；
- release-before-merge、reviewer 一般不跑本機全套測試等 local tightening；
- commander 一定不能做 code、lead 一定不能自實作等組織偏好；
- 所有案子都要某套 brainstorming／team-discuss／retro 方法；
- 我們的 model 別名清單、成本偏好、產品部署命令；
- 每項偶發教訓與每次 scope fidelity 的主觀判斷。

可以提供可選 starter role 與 recipe，但示例必須標為示例；使用者改掉角色名字／語氣後，公共流程仍能成立。

### 15.3 本輪不做

不做 hostile-seat OS isolation、不重寫 vendor CLI、不建立通用 workflow DSL／全功能 plugin 平台、不重構 Telegram（V6-7：連「功能需求重評估」也先不動）、不做 GUI IDE、不加 zero-review lane、不批次重寫歷史 review authority、不把任意外部 report 升格成 trusted receipt、不要求所有歷史 binary 與新 state 混寫。

## 16. 設計面向總覽

| 面向 | 落在本文件哪裡 |
|---|---|
| 產品定位與 operator 保留邊界 | §1.1、§1.2 |
| operator 裁決的唯一總表（P1／P2／A 類／V6 類） | §1.3 |
| 現有文件契約的缺口與上收策略 | §2 |
| 官方 workflow：責任與角色槽、政策 precedence | §3 |
| Instructions 與 Skills 的載入、生效與 ownership | §4 |
| Dispatch／review／completion 的閉環與錯誤可恢復 | §5 |
| no-CI 的狀態模型、切換與 consumer 同步 | §6 |
| 訊息、監看與跨 backend 的義務收斂 | §7、§10.3 |
| 治理、actor 分類與權限合成 | §8 |
| Decision board：refs／close／reopen／跨版本相容 | §9 |
| 恢復、worktree 與 backend 能力邊界 | §10 |
| GUI 與 external client | §11 |
| Fork、部署、升級與回復 | §12 |
| 工作包、v1 最小切面與依賴 | §13 |
| 驗收情境與量測 | §14 |
| Downstream 減法與退役判準 | §15 |
| issue 落點（V6-8）與上游 open issue 的處置追蹤 | §12.1、附錄 A |
| 部署用 binary 的建置來源（V6-9） | §10.2、§12.1 |

**核心交付承諾**：新使用者應該只需要定義「我的 agent 是誰、做什麼、專案怎樣驗收」，而不是先重建我們幾個月累積的「daemon 到底怎麼才用得對」。通用流程由產品維護，角色與專案差異由使用者擁有；兩者以可查、可驗、可恢復的契約相接。

## 附錄 A：issue 的處置

**新的 issue 一律開在本 repo**（V6-8）：daemon／MCP 的 bug、feature request、以及任何 fork 特有的問題，都在本 repo 開或查。上游 repo 保留 attribution，其既有 issue／PR 只作**歷史 provenance**——不是新問題的落點。

- **本 repo 的 issue tracker**：當下所有問題的權威清單；上游既有 issue 的處置分類結果也記在此（於 W0 建立，逐條列出處置與理由）。
- **上游既有 open issue**：可能就是待改的項目，須逐一處置（規則見 §12.1），在本 repo 重開並連回原 issue。上游之後新開的 issue 不再接收。
- 分類原則、退役條件與處置規則：§12.1。
- 本文件撰寫時（2026-10-04）的初判僅供對照，不具權威性；上游當時有 21 個 open issue。
