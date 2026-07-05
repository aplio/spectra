# spectra 開発再開 TODO

2026-07-05 時点の spectra / herdr / gargo コードベース調査に基づく。
参照: spectra v0.1.5 (~26k LoC), herdr v0.7.1, gargo v0.2.14。

> 進捗メモ (2026-07-05 20:35): **判断不要のタスクは全て実装完了**(13イテレーション・15コミット・テスト447→621・clippy 0維持)。
> 進捗メモ (2026-07-05 深夜): plugin基盤(P2)を実装完了 — manifest + argvコマンド + service supervision + agent manifest同梱 + `plugin.list`。[hooks]は統合しない判断で確定。
> 残りの判断待ちリスト:
> 1. sidebar 2段構成(専用agent panel)のUX (P4) — 現状はwindow list行のマーカーで代替済み
> 2. remote attachをherdr同等(バイナリ自動配布+checksum)まで作り込むか (P5)
>
> (2026-07-05 判断済み: kitty keyboard はパススルー相当で実装済み・kitty graphics は不採用・OSC 10/11 は(b)ホスト端末中継で実装済み)

---

## P7: バグ修正・改善 (2026-07-05 ユーザー報告)

- [ ] `spectra --version` 対応(gargo風) — **リポジトリでは実装済み**(コミット3a3a094、`spectra 0.1.5` を出力)。
      インストール済みバイナリが古いだけ。対応=リリース(Cargo.tomlのversion bump → push → tag → `--update`/install.sh で配布)。
      本バッチ完了時に 0.2.0 へ bump するか要確認 → とりあえず bump して push する方針
- [x] DONE cursor mode の `v` anchor toggle が効かない疑い — 原因判明: 移動キーが無条件に selection_anchor をクリアしていた。`visual` フラグ導入で v 選択は移動で伸長し、y はヤンク後 Normal へ戻る(vi 準拠)
- [x] DONE windowtree(SideWindowTree)の左端固定(x=0)ジオメトリのhardcode修正(P4の既知項目) — `SidebarRect`(origin+width)を導入し、compose・クリックhit-testing・pane xオフセット・reserve計算(effective_pane_cols)を全て同一rect経由に統一。挙動は従来どおり左端のみ(`SidebarRect::left_edge`が唯一の構成)、位置はパラメータ化済みで将来の右端/可変位置はrect生成の差し替えだけで済む
- [x] DONE enter/leave cursor mode のアクションを command palette で文脈フィルタ — `CommandPaletteContext` 導入でエントリ毎に表示可否を判定。palette は Normal からしか開けないため通常は enter のみ表示・leave は非表示(lock mode の enter/leave も同機構に統合)
- [ ] アーキテクチャ+テストカバレッジの再確認。カバレッジの穴にテストを実装(最後に実施)
- [x] DONE spectra内でClaude Codeを開くと下線が無駄に残ることがある(スクショ確認済み: プロンプト行に下線残留)。
      原因判明: SGRパーサがコロン下位パラメータを平坦化していた — `4:3`(波下線)が下線+イタリック、`4:0`(下線オフ)が「下線オン→全リセット」、`58:5:n`(下線色)の引数が独立コード(5=blink、4=下線!)として誤解釈され下線が固着。
      修正: vteのparamスライスを保ったままSGR解釈(`4:0`=off/`4:1..5`=on/`21`=二重下線(ECMA-48・xterm/kitty/ghostty準拠)/`38:48:58`のコロン・セミコロン両形式consume/`59` no-op)。レンダラ側は全リセット+再構築方式で元々正しいことをテストで確認
- [x] DONE drag中に status bar へ `shift+drag to select` 的なヒントを表示 — guest が mouse を掴む pane で左drag が転送された時のみ `shift+drag to select text` を2秒表示(drag継続中はリフレッシュ・通常paneのdragや単独クリックでは出さない・spectra側mouse無効時も出さない)
- [x] DONE ghosttyでURL(`https://...`)のcmd+clickがspectra経由だと効かない。
      原因判明: mouse capture干渉が本命。ghosttyはアプリがmouse reportingを有効にしている間リンク検知(hover/cmd+click)を完全に無効化する仕様(ghostty discussion #9514/#4618、回避はshift+cmd+click)で、spectraはclient起動時に無条件でEnableMouseCaptureしていた。
      OSC 8送出の差分フレーム破損疑いはテストで潔白を証明(部分再描画でもopen/close均衡・リンク再emit正常、regressionテスト3本追加)。
      修正: hostのmouse captureを動的化 — [mouse] enabled または active windowのguestがmouse reporting要求時のみcapture(server→clientへPassthrough制御メッセージで?1000h/l送出)。
      ※実ghosttyでの最終確認はユーザー依頼: シェルプロンプト表示中にcmd+clickが効くこと・guestがmouse使用中はshift+cmd+clickで開けること

## 現状サマリ（希望機能の実在チェック）

| 希望機能 | spectraの現状 |
|---|---|
| gargoのupdate/version | `--update` は**実装済み**(`src/upgrade.rs`, self_update crate, gargoと同設計)。`--version`/`--check` が**ない** |
| configでのkeybind | **実装済み**。`[prefix_bindings]`/`[global_bindings]` で上書き・unbind・prefix変更可 (`src/config.rs:28`, `src/input/keymap.rs:83`)。ただし固定enum約40アクションのみ、シェルコマンドは割当不可 |
| sidebar | **実装済み**(window list)。`prefix e` の `SideWindowTree` (`src/ui/render.rs:40`)。左端固定・拡張には形になる下地あり |
| herdrのremote attach | **簡易版実装済み**。`--remote <host>` がssh stdioトンネル+protocol versionハンドシェイクでリモートserverにattach(リモート設置済み前提・バイナリ自動配布は未) |
| agent integration (plugin形式) | **検知コアあり**。manifest駆動のAgentState検知(`src/agent/`, Claude 1種)+done導出+status bar `{agent}`トークン+`pane.list` の `agent` フィールド+sidebarマーカー+Claude Code hook統合(`spectra integration install claude`)+plugin基盤(`src/plugin/`, manifest/on_event/service/agent_manifest同梱) |

---

## P0: 小さくてすぐ効くもの

### 1. `--version` / `--check` (gargo移植)
- [x] DONE clap の `#[command(version)]` を付けた — `--version`/`-V` が動作
- [x] DONE `--check`: `upgrade.rs` に `UpdateCommand::Check` が実装済みだったのでCLI露出のみ追加。
      server起動中でも実行可(バイナリ置換しないため)。unit+E2Eテスト追加済み
- [x] DONE (任意) バックグラウンドTTL付き更新チェック + status barへの通知 — server起動時にdata dirの `update_check.toml`(TTL 24h・binary version不一致で無効)が新鮮なら即反映、
      なければ named thread("spectra-update-check")+mpsc で非同期チェックしループは try_recv で非ブロッキング回収。結果は `{update}` トークン(`update available: vX.Y.Z`)で露出(デフォルトformat未追加)、エラーはログのみでキャッシュしない
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
- [x] DONE **OSC 10/11 (fg/bg色 query)** — 戦略(b)を採用: client起動時にホスト端末へ1回クエリ(150ms上限・非tty時スキップ)→Helloで報告→server cacheから即答(rgb 16bit形式・BEL/ST終端はクエリに追従)・未取得時は無応答維持・guest側setは無視(v1)
- [x] DONE **OSC 8 hyperlink をgridで解釈** — `StyledCell` に `link: Option<Arc<str>>` を追加し、OSC 8 の
      URIをアクティブリンクとして追跡して印字セルにスタンプ(URI上限2083B・passthrough転送は従来通り維持)。
      レンダラはセルリンクを自前URL検知より優先してOSC 8でラップ出力
- [x] DONE kitty keyboard protocol — パススルー相当の軽量実装: pane毎(main/alt画面別)のflagスタック(push/pop/set/上限16)+`CSI ? u` クエリ応答+bit1(disambiguate)/bit8(report-all)のCSI-uエンコード(bit2/4/16は追跡のみ)。クライアントは `supports_keyboard_enhancement` 検出時のみ DISAMBIGUATE|REPORT_ALTERNATE_KEYS をpush。フル実装(イベント種別/associated text等)ではない
- [x] 不採用 kitty graphics — ユーザー判断で不採用(2026-07-05)。必要になったら再検討

herdr方式の学び: OSCトラッカーを**VTパーサと分離した独立のバイトストリーム監視**として実装している(パーサに手を入れずに追加できる)。spectraでもterminal_state.rsを肥大化させず `session/osc_tracker.rs` 的に分けるのが良い。

---

## P2: Plugin基盤 = JSON-RPC socket API（agent integrationの前提）

> APIメソッド表面はherdr式(「CLI=APIラッパー」・coreと分離したthin adapter)で確定・実装済み(2026-07-05)。
> plugin manifest形式もherdr式(manifest + argvコマンド・SDKなし・API socketが統合面)で確定し実装済み(2026-07-05) — **P2は完了**。

herdrの拡張モデルが秀逸: **TUI用のバイナリprotocolとは別に、改行区切りJSON-RPCのsocket APIを立てる** (`src/api/server.rs`)。
plugin = 「`herdr-plugin.toml` マニフェスト + 任意言語のargvコマンド」で、SDK不要。CLI自体がAPIのラッパー。

- [x] DONE **第2ソケット** — `spectra-api.sock` を同runtime dirに追加(`src/ipc/socket_path.rs::api_socket_path`)。
      改行区切りJSON-RPC(`{id, method, params}` → `{id, result|error}`)。server busy-poll loopに
      nonblocking accept/read/flushを統合、複数同時接続可・1行1MiB上限・parse errorは-32700で接続維持。
      dispatchは純関数 `api::dispatch(&App, &str) -> String`(&Appのみ=読み取り専用保証)。unit+E2Eテストあり
- [x] DONE **最初のメソッドセット** — 読み取り系 `session.list` / `pane.list`(session_idフィルタ・title解決・agentフィールド) /
      `pane.read`(デフォルト可視画面・`lines:N`でscrollback込み末尾N行) に加え、herdr式で確定した書き込み系
      `pane.send_keys`(PTYへraw text) / `pane.split`(新pane id返却・CLI split-windowと同一経路) / `agent.report`(外部報告agent状態、30s TTLで検知を上書き) と
      `events.subscribe`(hook 6種のブリッジ+`agent.changed`を接続毎フィルタでpush、キューは1024上限)を実装済み。
      dispatchは`&mut App`化したがapi.rsはthin adapterのままcoreと分離(2026-07-05)
- [x] 一部DONE CLIサブコマンドをこのAPIのラッパーとして生やす — 汎用 `spectra api <method> [json]` を実装(agent/スクリプト向けの脱出ハッチ)。`--follow`でevents.subscribeのeventラインを追尾表示。`spectra pane read` 等のpretty wrapperは必要になったら
- [x] DONE plugin manifest (`spectra-plugin.toml`) — `src/plugin/` に実装(2026-07-05)。plugin = 「manifest + 任意言語のargvコマンド」でSDK不要、API socketが統合面。
      発見場所は `$XDG_CONFIG_HOME/spectra/plugins/<name>/`(優先) と `$XDG_DATA_HOME/spectra/plugins/<name>/`、server起動時+config reload時に再スキャン(不正manifestはログしてスキップ)。
      3機能: `[[on_event]]`(購読イベント発火でargvを一発起動・イベントJSONをstdin配送・`{event}`プレースホルダ・`SPECTRA_EVENT`/`SPECTRA_API_SOCKET` env)、
      `[service]`(常駐子プロセス: capped backoff 1s→30sで再起動・60秒内5回で打ち切り・stdout/stderrは plugin dirの `service.log`(起動時>1MiBで切り詰め)・drop guardでshutdown時kill)、
      `[agent_manifest]`(検知manifest同梱→registryへマージ)。`plugin.list` APIメソッドで一覧可
- [x] DONE 既存 `[hooks]` はこのイベント購読の特殊ケースとして統合を検討 — **統合しない判断**(2026-07-05): hooksはシェル1行の軽量版としてそのまま維持、
      pluginが構造化上位互換(argv・stdinペイロード・常駐・配布)。同じ `push_api_event` 経路から両方に配送されるだけで実装も干渉しない

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
- [x] DONE P2のAPI経由 `pane.report_agent` メソッド+公式hook — `agent.report` として実装済み(2026-07-05): kindサニタイズ(小文字英数+dash・32字上限)、報告後30秒は画面検知を抑止して報告値を優先、期限後はmanifest検知が再開して上書き。seen/done導出・通知は検知経路と同一。
      Claude Code hook統合も実装済み(2026-07-05): 全paneに `SPECTRA_API_SOCKET`/`SPECTRA_PANE_ID`/`SPECTRA_SESSION_ID` をexport、埋め込みPOSIX-shスクリプト(`src/integration/assets/claude/spectra-agent-state.sh`, Stop→idle/Notification→blocked(permission)or idle(入力待ち)/UserPromptSubmit・PreToolUse→working、transport は nc -U→python3 フォールバック・全失敗silent)+`spectra integration install/uninstall claude`(settings.jsonへ冪等マージ・atomic write・初回 .bak・--dry-run diff)
- [x] DONE sidebar表示 + done(=idle&&未閲覧)導出 — herdr式に done は保存せず導出: working/blocked→idle 遷移を非フォーカスpaneで検知したら unseen、描画時にフォーカスpaneを seen 化(閲覧中のpaneは決してdoneにならない)。`AgentDisplayState`(unknown/idle/done/working/blocked)として `{agent}` トークンと `pane.list` の `state` にも露出
- [x] DONE (任意)ホスト端末へのdesktop notification — agent状態変化でOSC 9 (`ESC ]9;msg BEL`)を全attachedクライアントへブロードキャスト。`[agent] notify = "off"|"blocked"|"all"`(default "blocked"、"all"はdone通知も追加)、
      閲覧中paneは通知せず・pane×state毎に1回debounce(状態が離れて戻ると再arm)。対応端末はghostty/iTerm2/wezterm(v1、herdr式の端末別出し分けは未)
- [x] DONE plugin形式にするなら: 検知マニフェスト+hookスクリプトを plugin manifest に同梱して配布、という切り方が自然 — P2のplugin基盤の `[agent_manifest]` 同梱で実現(2026-07-05)。
      plugin付属のagent検知manifestはbuiltin+先行plugin優先で runtime registry(`App.agent_manifests`, Arc swap)にマージされ、再コンパイルなしにユーザー定義agent検知が可能。
      検知manifestのホットリロードもplugin再スキャン(config reload)経由で実質実現

---

## P4: Sidebar拡張（agent panel）

- [ ] 既存 `SideWindowTree` を汎用sidebarに拡張: window listセクション + agent panelセクションの2段構成
      (herdr `src/ui/sidebar.rs`: ratio-based分割・ドラッグ可能な仕切り・`prefix+b` トグル)
- [x] DONE agent状態の行表示: 状態ドット(赤●blocked / 黄●working / cyan●done未読 / 緑✓idle)を
      window list行の右端にマーカー表示(window内paneの最悪状態を集約、divider内に収まる幅計算)。
      専用の2段agent panel/ドラッグ仕切りは未(上の項目)
- [x] DONE 注意: 現状 `SideWindowTree` は左端固定(x=0)のジオメトリがハードコード。汎用化するならreserve計算を先に整理 — `SidebarRect` で整理済み(P7参照)。reserve = `rect.pane_x_offset()` に一本化
- [x] DONE herdrの規律を借りる: agent状態(`AgentStatus`/seen)はサーバ側 `App` が正データとして保持し、
      マーカー(`AgentIndicator`/`AgentDisplayState`)は描画スナップショット構築時に純粋に導出。永続化もしない

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
- [x] DONE 簡易版: 隠し `remote-client-bridge` サブコマンド+`--remote <host>` のssh stdioトンネル+`SPECTRA_REMOTE_SSH_CMD` テストシームで実装(リモート設置済み前提・バイナリ自動配布はやらない)
- [x] DONE protocolのversion negotiation: `Hello` に `protocol_version` を追加(client常送・不一致はErrorで切断・レガシーNoneは許容)。RenderのANSI差分構造はそのままremoteで流用
- [ ] ControlMaster=auto + keepaliveの managed ssh config (herdr `:1595`)は後回しで可

---

## P6: アーキテクチャ改善（機能ではないがherdrに劣る点）

- [x] DONE **イベントループのepoll化** — `polling` crate採用(tokioなし・最薄)。中央`Poller`が両listener+全client/API streamを監視し、fdを持たないmpsc生産者(PTY readerスレッド・update-checkスレッド)は`wake::notify()`(プロセスグローバルwaker)で起床。pollingはoneshot配送なので毎wait前に`rearm_poll_interest`ヘルパーで全ソースのinterestを一括再arm(再arm漏れによるstallを構造的に排除)、書き込みはWouldBlock時のみwritable監視。waitのtimeoutは`App::next_deadline`(sync-output hold期限・agent検知スロットル・status message期限)と250ms heartbeat上限のmin。アイドルCPU実測(release,10s): 152ms→3.2ms(~47倍改善)、`tests/idle_cpu_e2e.rs`で回帰ゲート(5sで50ms上限)
- [x] DONE **unwrap/expect監査** — 実棚卸しでprod経路は6箇所のみ(~149はcfg(test)込みの過大見積り)。terminal::setup×2をio::Result化・reflowのunwrap×1をis_some_and化で修正、不変条件expect×3は理由付き#[allow]で意図明示、lock系prod使用ゼロ。lint gateはlib.rs/main.rsに`#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]`でcrate全体に適用(テストmodは非対象)
- [x] DONE **god object分割** — 純粋なコード移動で2ファイルを分割(挙動・公開API変更なし・全726テスト無修正パス)。`app/mod.rs` 3643行→991行+`clients`(504)/`input`(609)/`actions`(508)/`render_snapshot`(570)/`agents`(248)/`api_support`(239)、`terminal_state.rs` 3646行→ディレクトリ化 `mod.rs`(328)+`grid`(1174, TerminalGrid本体+Perform一体で維持)/`reflow`(395)/`modes`(178)/`passthrough`(92)/`tests`(1499)
- [x] DONE alt-screen resizeがnaive(reflowなし) — 保存中のprimary画面をalt中のresizeでも通常経路と同じsoft-wrap reflowで追随(`reflow_saved_screen`)・alt画面自体はclip/pad維持・連続resize合成もtwin-grid同値でテスト
- [x] DONE **SCM_RIGHTSによるlive handoff** — `spectra server-handoff` + SCM_RIGHTSでPTY master fdを新serverへ移送(≤32/batch・上限64)+pane毎≤8KB replayで画面復元+`--update`はserver稼働中でも成功しhandoffヒントを表示。v1制約: クライアント接続中は拒否(自動再接続なし・paneは無傷)・replay超のscrollbackは引き継がない・final ack前の失敗は旧server無傷で継続
- [ ] IPCのバイナリ化(bincode+length-prefix)は**急がない** — NDJSONで困ってから。protocol versionフィールドはP5(remote attach)で導入済み
- [x] DONE keybindの拡張 — `run:` プレフィクスで任意シェルコマンドをbind可・hooks実行基盤を流用・paletteには出さない

## spectraがherdrに勝っている点（維持すべき資産)

- テストが厚い: inline unit (app/tests.rs 5410行) + E2E ~3250行(attach/detach, render snapshot, latency)。この規律は維持
- 依存が軽い: Zig/FFI/vendorなしの純Rust単一crate。libghostty-vt追従の保守コストを負っていない
- 自前VTグリッドは(穴はあるが)全部自分のコードなので、P1の穴埋めは足すだけ
- command palette + SQLite recency、tmux DCS passthroughなど独自機能あり

## 実装順の提案

P0(半日) → P1のmouse/paste/2026(agent以前に日常品質) → P2 API → P3 agent(まずClaude 1種) → P4 sidebar統合 → P5 remote → P6は随時。
