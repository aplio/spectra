# spectra 開発再開 TODO

2026-07-05 時点の spectra / herdr / gargo コードベース調査に基づく。
参照: spectra v0.1.5 (~26k LoC), herdr v0.7.1, gargo v0.2.14。

> 進捗メモ (2026-07-05): P0 完了。`⏸ 要判断` マークの項目は設計/採否の判断が必要なため
> ユーザー確認まで後回し。それ以外は上から順に実装を継続中。

## 現状サマリ（希望機能の実在チェック）

| 希望機能 | spectraの現状 |
|---|---|
| gargoのupdate/version | `--update` は**実装済み**(`src/upgrade.rs`, self_update crate, gargoと同設計)。`--version`/`--check` が**ない** |
| configでのkeybind | **実装済み**。`[prefix_bindings]`/`[global_bindings]` で上書き・unbind・prefix変更可 (`src/config.rs:28`, `src/input/keymap.rs:83`)。ただし固定enum約40アクションのみ、シェルコマンドは割当不可 |
| sidebar | **実装済み**(window list)。`prefix e` の `SideWindowTree` (`src/ui/render.rs:40`)。左端固定・拡張には形になる下地あり |
| herdrのremote attach | **なし**。Unix socketローカルのみ |
| agent integration (plugin形式) | **検知コアあり**。manifest駆動のAgentState検知(`src/agent/`, Claude 1種)+status bar `{agent}`トークン+`pane.list` の `agent` フィールド。hook/通知/sidebar/plugin配布は未 |

---

## P0: 小さくてすぐ効くもの

### 1. `--version` / `--check` (gargo移植)
- [x] DONE clap の `#[command(version)]` を付けた — `--version`/`-V` が動作
- [x] DONE `--check`: `upgrade.rs` に `UpdateCommand::Check` が実装済みだったのでCLI露出のみ追加。
      server起動中でも実行可(バイナリ置換しないため)。unit+E2Eテスト追加済み
- [ ] (任意) バックグラウンドTTL付き更新チェック + status barへの通知
      (gargo `src/command/update_check_runtime.rs` + `UpdateCheckCache`, TTL 24h, `update_check.toml` にキャッシュ)
- [x] DONE テストシーム: `SPECTRA_TEST_UPDATE_SOURCE=mock` / `SPECTRA_TEST_UPDATE_STATE` が既に存在(gargoと同設計)。確認のみ

### 2. FIXME.md の削除/書き直し
- [x] DONE 削除した(untrackedだったのでgit履歴にも残らない)。設計メモは本TODOに集約

---

## P1: OSC / VT対応の穴埋め（agent連携とTUI互換性の土台）

spectra最大の機能ギャップ。VT処理は `src/session/terminal_state.rs` (自前grid + vte crate)。
herdrはlibghostty-vt(vendored, Zig FFI)に委譲し、足りない分を `src/pane/osc.rs` のトラッカーで補う構成。

- [x] DONE **ゲストのmouse mode (?9/1000/1002/1003/1006) を尊重** — pane毎に追跡し、カーソル下のpaneが
      mouse reportingを要求していればSGR/レガシー両エンコードで転送(spectraの[mouse]設定と独立に動作)。
      Shift押下はホスト側処理へのバイパス(xterm/tmux慣行)。プロトコル毎のイベント種フィルタ実装済み(X10=pressのみ等)
- [x] DONE **bracketed paste (?2004) をpane毎に追跡** — TerminalGridで?2004h/lを追跡し、
      ゲストが有効化した時のみpasteを `ESC[200~`/`ESC[201~` でラップ。埋め込みend markerは除去(paste injection対策)
- [x] DONE **synchronized output (?2026)** — pane毎に追跡し、active windowのpaneがhold中は
      server loopがフレーム送出を遅延(needs_renderは保持)。150ms上限で暴走ガード(`SYNC_OUTPUT_TIMEOUT`)
- [x] DONE **OSC 52 inbound** — osc_dispatchで受けてbase64デコードし、全attachedクライアントへ
      OSC 52フレームをブロードキャスト(tmuxのset-clipboard相当)。256KiB上限・クエリ(`?`)は無応答で無視。
      vteはstd featureでOSCペイロード無制限Vecのためdispatch側でcap
- [x] DONE **OSC 133 (semantic prompt)** — `133;A` (prompt start) の絶対行をpane毎に追跡
      (`last_prompt_abs_row`)。P3 agent検知の「最後のプロンプト以降」region計算に使う。B/C/Dマークは必要になったら
- [ ] ⏸ 要判断 OSC 10/11 (fg/bg色 query) — 応答戦略の判断待ち: (a)固定デフォルト応答(ダーク/ライト誤検知リスク),
      (b)ホスト端末へ問い合わせ中継(実装複雑・非同期), (c)現状維持(無応答=アプリ側タイムアウト)。推奨は(b)だが工数大
- [x] DONE **OSC 8 hyperlink をgridで解釈** — `StyledCell` に `link: Option<Arc<str>>` を追加し、OSC 8 の
      URIをアクティブリンクとして追跡して印字セルにスタンプ(URI上限2083B・passthrough転送は従来通り維持)。
      レンダラはセルリンクを自前URL検知より優先してOSC 8でラップ出力
- [ ] ⏸ 要判断 kitty keyboard protocol (herdr `src/pane/kitty_keyboard.rs`) — 対応範囲(パススルーのみか完全実装か)の判断待ち
- [ ] ⏸ 要判断 kitty graphics — herdrはフルサポート(`src/kitty_graphics.rs`, 32MBフレーム上限)だが実装コスト大。採否の判断待ち

herdr方式の学び: OSCトラッカーを**VTパーサと分離した独立のバイトストリーム監視**として実装している(パーサに手を入れずに追加できる)。spectraでもterminal_state.rsを肥大化させず `session/osc_tracker.rs` 的に分けるのが良い。

---

## P2: Plugin基盤 = JSON-RPC socket API（agent integrationの前提）

> ⏸ 一部要判断: APIメソッド表面とplugin manifest形式は互換性を縛る設計判断なので、
> 着手前に方針確認したい(herdr式の「CLI=APIラッパー・manifest+argvコマンド」方式で良いか)。
> `pane.read`/`pane.list` 等の読み取り系メソッドは判断不要と思われるため先行実装可。

herdrの拡張モデルが秀逸: **TUI用のバイナリprotocolとは別に、改行区切りJSON-RPCのsocket APIを立てる** (`src/api/server.rs`)。
plugin = 「`herdr-plugin.toml` マニフェスト + 任意言語のargvコマンド」で、SDK不要。CLI自体がAPIのラッパー。

- [x] DONE **第2ソケット** — `spectra-api.sock` を同runtime dirに追加(`src/ipc/socket_path.rs::api_socket_path`)。
      改行区切りJSON-RPC(`{id, method, params}` → `{id, result|error}`)。server busy-poll loopに
      nonblocking accept/read/flushを統合、複数同時接続可・1行1MiB上限・parse errorは-32700で接続維持。
      dispatchは純関数 `api::dispatch(&App, &str) -> String`(&Appのみ=読み取り専用保証)。unit+E2Eテストあり
- [x] 一部DONE **最初のメソッドセット** — 読み取り系 `session.list` / `pane.list`(session_idフィルタ・title解決) /
      `pane.read`(デフォルト可視画面・`lines:N`でscrollback込み末尾N行) は実装済み。
      書き込み系 `pane.send_keys` / `pane.split` と `events.subscribe` は未実装(⏸ 要判断につき方針確認後)
- [ ] CLIサブコマンドをこのAPIのラッパーとして生やす (`spectra pane read <id>` 等) — agentからの操作面にもなる
- [ ] plugin manifest (`spectra-plugin.toml`): 名前/コマンド/購読イベント。イベント発火でargv起動 or 常駐プロセスにNDJSON配送
- [ ] 既存 `[hooks]` はこのイベント購読の特殊ケースとして統合を検討

---

## P3: Agent integration（本命）

herdrの実装 (`src/detect/`) は**4層の独立シグナル**の合成。状態は idle/working/blocked/done/unknown。

1. **プロセス名マッチ** — pane内フォアグラウンドプロセスのargvからagent種別を判定 (`src/detect/mod.rs:143`)
2. **画面下端のマニフェスト検知(最重要)** — agent毎のTOML (`src/detect/manifests/*.toml`, 18種)。
   `priority` + `contains`/`regex`/`any`/`all`/`not` ルールを、pane最下部バッファのregion
   (`prompt_box_body`, `bottom_non_empty_lines(N)`, `osc_title` 等)に適用。ホットリロード可・`agent explain` デバッグコマンドあり
3. **OSC受動監視** — OSC 0/2 title, OSC 9 progress, 独自OSC 21337 status (`src/pane/osc.rs:459,681`)
4. **公式hook(任意)** — Claude Codeのhookスクリプトが `HERDR_SOCKET_PATH`/`HERDR_PANE_ID` 経由でJSON APIに
   `pane.report_agent_session` を叩く (`src/integration/assets/claude/herdr-agent-state.sh`)。会話resumeにも使う

「done」の扱いが上手い: **done = idle && 未閲覧**。paneをフォーカスすると seen=true になり done→idle に落ちる(状態として保存しない)。

spectraタスク:
- [x] DONE `AgentState` enum + pane毎の検知結果保持 — `unknown/idle/working/blocked` + `AgentStatus{kind,state,since}` を `ManagedSession` 毎に保持。tickで出力変化paneのみ・pane毎200msスロットルで再検知、`pane.list` の `agent` フィールドと status `{agent}` トークン(デフォルトformatには未追加)で露出
- [x] DONE マニフェスト駆動ルールエンジン — TOML(`priority`+`contains/regex/any/all/not`× region `bottom_non_empty_lines(N)`/`osc_title`)を `src/agent/manifest.rs` で実装。Claude 1種の組み込みmanifest(`src/agent/manifests/claude.toml`, include_str!)のみ・ホットリロードと `agent explain` は未
- [x] DONE プロセス名フォールバック検知 — Linux-only best effort。`PaneBackend::child_pid` → `/proc/<child>/stat` tpgid → `/proc/<tpgid>/cmdline` argv[0] basename を `process_names` と照合。失敗は全てNone(パニックなし)
- [ ] P2のAPI経由 `pane.report_agent` メソッド + Claude Code hookスクリプト(integration install コマンド)
- [ ] 状態変化 → sidebar 表示 + (任意)ホスト端末へのdesktop notification(herdr `src/terminal_notify.rs` は端末種別ごとにOSC通知方式を出し分け)。done(=idle&&未閲覧)導出も未
- [ ] plugin形式にするなら: 検知マニフェスト+hookスクリプトを plugin manifest に同梱して配布、という切り方が自然

---

## P4: Sidebar拡張（agent panel）

- [ ] 既存 `SideWindowTree` を汎用sidebarに拡張: window listセクション + agent panelセクションの2段構成
      (herdr `src/ui/sidebar.rs`: ratio-based分割・ドラッグ可能な仕切り・`prefix+b` トグル)
- [ ] agent panel行: agent名 + 状態ドット(herdr: 赤●blocked / 黄spinner working / teal●done未読 / 緑✓idle)
- [ ] 注意: 現状 `SideWindowTree` は左端固定(x=0)のジオメトリがハードコード。汎用化するならreserve計算を先に整理
- [ ] herdrの規律を借りる: sidebar等のUI状態は**クライアント/描画側のプレゼンテーション状態**であってサーバの正データにしない

---

## P5: Remote attach (SSH stdio bridge)

> ⏸ 一部要判断: 簡易版(リモート設置済み前提)で止めるか、herdr同等(バイナリ自動配布+checksum照合)
> まで作り込むかの判断待ち。簡易版の実装自体は判断不要で進められる。

herdr方式 (`src/remote/unix.rs`, 96KB): **TCPポートを開けず、ssh -T の stdin/stdout でクライアントsocketをトンネル**。

フロー:
1. リモートに同一versionのバイナリを確認/自動インストール(checksum照合)
2. リモートserverをprotocol version確認付きで起動
3. ローカルにUnixListener(0700)を立て、接続毎に `ssh -T host 'exec herdr remote-client-bridge'` を張り、socket⇔ssh stdioを2スレッドでコピー
4. リモート側bridgeがremote serverのclient socketに中継
5. ローカルclientは `terminal-ansi` エンコーディング(server側でANSI差分化)で接続 → 細い回線でも効率的

spectraタスク:
- [ ] まず簡易版: 「リモートに既にspectraがある前提」で bridge サブコマンド + ssh stdioトンネルだけ実装(herdrのバイナリ自動配布はやらない)
- [ ] spectraのRenderは既にANSI差分をserver側で作っているので、remote向きの構造は実は既にある(protocolのversion negotiationだけ足す)
- [ ] ControlMaster=auto + keepaliveの managed ssh config (herdr `:1595`)は後回しで可

---

## P6: アーキテクチャ改善（機能ではないがherdrに劣る点）

- [ ] ⏸ 要判断 **イベントループのepoll化** — 現状1ms sleepのbusy-poll (`src/runtime/server.rs:22,67`)。アイドル時CPUを常時食う。mio/pollingへ。herdrはtokio multi-thread。依存追加(mio/polling/tokioのどれか)と改修範囲が大きいため方針確認したい
- [ ] **unwrap/expect監査** — 非テストコードに~149箇所。herdrは「prod unwrap禁止」を規約で強制
- [ ] **god object分割** — `app/mod.rs` 2838行 / `terminal_state.rs` 2685行。herdrの「AppState=純データ、render=純関数、runtime分離」規律 + `assert_invariants_for_test()` パターンが参考になる
- [ ] alt-screen resizeがnaive(reflowなし, `terminal_state.rs:354`)
- [ ] ⏸ 要判断 (面白い候補) **SCM_RIGHTSによるlive handoff** — herdrはPTYのfdをUnix socket越しに新serverへ渡してpaneを殺さずserver更新 (`src/server/handoff.rs`)。self-updateと組み合わせると「動作中に無停止アップグレード」が可能に。採否の判断待ち
- [ ] IPCのバイナリ化(bincode+length-prefix)は**急がない** — NDJSONで困ってから。ただしprotocol versionフィールドだけは先に入れておく(remote attachで必須)
- [ ] keybindの拡張: 固定enumに加えて任意シェルコマンド/APIメソッドをbind可能に(tmuxユーザの期待値)

## spectraがherdrに勝っている点（維持すべき資産)

- テストが厚い: inline unit (app/tests.rs 5410行) + E2E ~3250行(attach/detach, render snapshot, latency)。この規律は維持
- 依存が軽い: Zig/FFI/vendorなしの純Rust単一crate。libghostty-vt追従の保守コストを負っていない
- 自前VTグリッドは(穴はあるが)全部自分のコードなので、P1の穴埋めは足すだけ
- command palette + SQLite recency、tmux DCS passthroughなど独自機能あり

## 実装順の提案

P0(半日) → P1のmouse/paste/2026(agent以前に日常品質) → P2 API → P3 agent(まずClaude 1種) → P4 sidebar統合 → P5 remote → P6は随時。
