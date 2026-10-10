# dgxtop

[English](../README.md) | [繁體中文](README.zh-TW.md) | [简体中文](README.zh-CN.md)

> NVIDIA DGX と GPU 搭載 Linux ホスト向けの、安定して効率的なローカルターミナルモニター：
> CPU、GPU、メモリ、ディスク、ネットワーク、プロセス。説明可能なマルチソース調停、正確な履歴、
> 保護されたプロセス制御を備えます。

dgxtop v2 は [`spec/dgxtop-v2/`](../spec/dgxtop-v2/README.md) の仕様に基づいて全面的に書き直されました。
偽の 0 は決して表示しません。読み取れなかった値は *unsupported*（非対応）、*read failed*（読み取り失敗）、
*stale*（古い）、*source conflict*（ソース競合）として、取得元とともに表示されます。

## 特長

- **分離されたノンブロッキング収集。** 各 live データソース（procfs、hwmon、thermal、powercap、NVML、
  network、filesystem）はそれぞれ常駐する adapter-host プロセスで動作します。ドライバ呼び出しが止まっても
  （例：NVML のハング）影響はその host に限られ、CPU・メモリ・UI は更新を続けます。タイムアウトした host は
  上限付きバックオフで再起動されるか隔離（quarantine）され、無制限に再生成されることはありません。
- **説明可能なソース調停。** 2 つのソースが同じメトリクスを報告する場合（例：CPU パッケージの coretemp と
  thermal zone）、証明済みの同一ソースの別名を除外し、独立したソースだけを比較し、許容差を超えたら平均化も
  黙った選択もせず「ソース競合」と表示します。*Sources* ビューは各候補、選択・除外の理由、経過時間、取得元を示します。
- **誠実な単位と品質。** 読み取り状態、データ種別（計測／導出／推定／シミュレーション）、品質、鮮度は別々の軸です。
  検証されていない単位（NVML PCIe の「KB/s」など）を推測で変換したり、使用率から帯域幅を捏造したりしません。
- **正確な履歴。** レートは正確な `u64` カウンタ差分を実経過時間で割って求めます。ウィンドウ合計はウィンドウに
  完全に含まれる区間だけを数えます（分をまたぐ区間は正確な *bridge* として保存）。欠損・カバレッジ・ソース切替は可視化されます。
- **上限のあるリソース。** 管理対象メモリは単一のグローバル予算（既定 256 MiB）で、すべてのキューにバイト数と件数の上限があります。
  スレッド数は固定、再描画は変化時のみで最大 4 fps、ターミナル出力はノンブロッキングで、遅い端末でも入力が止まりません。
- **保護されたプロセス制御。** `K` で選択プロセスへの SIGTERM を要求します。同一ユーザーのみ、root セッションは拒否、
  PID 1・dgxtop 自身・その host は対象外、10 秒以内に有効な 1 回限りの確認が必要、シグナル送信**前**に永続的な監査記録を書き込み、
  保持済みの `pidfd` 経由で送信します（PID 再利用の競合なし）。`SIGKILL` は既定で無効です。結果は
  「シグナル送信済み」と「終了を確認」を区別します。

## ビューとモジュール

10 個の表示モジュール（`cpu`、`gpu`、`memory`、`disk`、`network`、`processes`、`history`、`sources`、
`settings`、`actions`）があります。タブは Overview、GPU、History、Settings です。確認待ちのプロセス操作はどのビューでもバナーで表示し、結果（送信済み・実行中・終了・不明）は通知で表示します。

- **Overview：** 上から 4 段構成です。
  - **CPU とメモリを横並び（メモリが広め）。** CPU は温度・電力・最大周波数（収まるときは同じ行に負荷とタスク数）、使用率のバーとその下のグラフ、usr/sys/iowait/steal の内訳を表示し、メモリは使用量、used のバーとその下のグラフ、swap・buffers・cache・利用可能量を表示します。CPU パネルのクリックまたは `c` で全センサーとコアごとのバーを追加します。ユニファイドメモリの GPU（DGX Spark）では、GPU プロセスが使う RAM を別の色で示し、その量も表示します。
  - **GPU。** GPU ごとに温度・電力・クロック・ファンの行、使用率のバーとその下のグラフ、続いてメモリのバー（`VRAM`、ユニファイドメモリでは GPU プロセスが使う RAM を示す `RAM`）を表示します。端末が低いときは GPU を優先して広げます。
  - **ディスク I/O とネットワークを横並び。** それぞれ表（ディスクは載っているファイルシステムの容量付き）と、最も忙しいデバイス（タイトルに表示）の読み書きまたは送受信のレートです。
  - **GPU プロセス表**が残りの行を使います。GPU を使うプロセスだけを表示し、`TYPE` 列は `nvidia-smi` と同じ表記で、その GPU 上のコンテキストが計算（`C`）かグラフィックス（`G`）か両方（`C+G`）かを示します。選択・ソート・フィルター・終了はここで行います。

  グラフはひと目で読めることを重視しています。ブロックの列で、1 列が 1 サンプル、最新が右端です。各列はその値に応じたバーの色になり、負荷の高い時間帯はひと目で赤く見えます。使用率とメモリは固定スケールです。ディスクとネットワークのレートは自身のピークでスケールし、ピーク値を横に表示します。
- **GPU：** 1 台ずつ表示します。上に 4 本のリアルタイムバー（使用率・メモリ・温度・電力）、その下にその GPU のリアルタイムと詳細の情報をすべて並べます。データはすべて NVIDIA ドライバ（NVML）とドライバ・CUDA toolkit のファイルから読み取り、ネットワークにも接続せず、コマンドも実行しません。情報は分類ごとに上から下へ読む「名前　値」のブロックに並び、比べる値は上下に揃います。ブロックは端末に収まる最少の、高さを揃えた列に並びます。
  - **State：** P-state、throttle の理由、原因ごとの累計時間。
  - **Utilization：** メモリコントローラ、エンコーダ、デコーダ。
  - **Clocks：** graphics・SM・memory・video の各クロック、現在値と最大値を並べて表示。
  - **Memory：** 総量、使用量、free、reserved、プロセスのメモリ、BAR1、バス幅と理論ピーク帯域。
  - **PCIe：** link と理論帯域の現在値と最大値を並べて表示、transmit の真下に receive、replay。
  - **Thermal：** 温度とファン（複数ファンの GPU では全ファン）、続いてデバイス固有の温度しきい値を低い順に。
  - **Power：** 電力とドライバの瞬時値（Ampere 以降の GPU では電力は NVML の 1 秒平均）、上限、他の制限で実効上限が下がっているときは設定上限も、既定の上限、設定可能な範囲、ドライバ読み込み以降の消費電力量。
  - **CUDA：** ドライバが対応する CUDA バージョン、compute capability（`sm_120`）、インストール済み toolkit とそのパス・`nvcc`。
  - **Driver：** バージョン、open／proprietary カーネルモジュール、NVML、persistence、compute mode。
  - **Device、PCI & firmware：** アーキテクチャ、ブランド、CUDA コア、board part、シリアル、UUID、デバイスノード；PCI の識別情報、VBIOS・InfoROM・GSP ファームウェア。
  - **Health：** GPU が対応していれば ECC・retired pages・NVLink のカウンタ。
  - **Topology：** GPU に最も近い NUMA ノードと CPU、ほかの各 GPU への PCIe 経路（`nvidia-smi topo -m` の `PIX`〜`SYS`）と、相手のメモリを直接読み書きできるか（P2P）。P2P がないと GPU 間のコピーはシステムメモリを経由します。

  GPU が報告しないデータには行を設けません。端末が低くすべてのブロックが収まらないときは、表示しきれないブロック名を上に示します。各 GPU を使うプロセスは overview のプロセス表にあります（`g` で 1 台に絞り込み）。
- **History：** overview の各リソースをより長い時間範囲（5 分〜24 時間、`w`/`W` で切り替え）で、監視ダッシュボードのように表示します。パネルは 2 つずつ並びます：CPU とメモリ、GPU の使用率とメモリ、GPU の温度と電力（GPU ごとに 1 本）、最も忙しいディスクの読み書きとインターフェースの送受信（各組は 1 つのスケールを共有）。各ストリップにはスケール、時間範囲のブロック列（平均をレベルの色で、ピークを上に淡い帯で、記録のない時間を点線のベースで表示）、現在値・平均・ピークの凡例があり、各列のパネルの下に時間軸があります。履歴はディスクに保存され（`history.sqlite3`、既定でオン）、以前の実行や再起動前のデータも読み戻して同じ時間軸に描きます。ウィンドウバーにはどれだけ記録済みかを表示します（記録は dgxtop の実行中のみ）。時間軸の下には各 GPU の直近 1・6・12・24 時間の統計（使用率・メモリ・温度・電力の平均とピーク、電力量）を、データが増える時間範囲だけ表示します。設計の分析は [docs/process/v2-rewrite/history-redesign.md](process/v2-rewrite/history-redesign.md) にあります。

レイアウトは端末サイズに追従します：

- 80×24 以上で完全なレイアウト；
- 80×24 未満で簡易サマリー；
- 40×10 未満でサイズ警告（q で終了可能）。

## インストール

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

インストーラは x86_64 または aarch64 の GNU ビルドを対象ファイルシステム上の一時ディレクトリにダウンロードし、
`SHA256SUMS` とアーカイブ構成を検証し、`dgxtop --version --verbose` で読み込めることを確認してから実行ファイルを
アトミックに置き換え、旧版を `dgxtop.previous` として残します。`./install.sh --rollback` で元に戻せます
（設定と状態は変更されません。送信済みのシグナルは取り消せません）。musl 版への自動フォールバックはありません。

ソースからのビルド（`rust-toolchain.toml` で Rust 1.92.0 を固定）：

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release --locked
./target/release/dgxtop
```

## 使い方

```bash
dgxtop                       # ライブモニター
dgxtop -i 0.5                # 今回の実行のみ 500 ms 間隔（保存済み設定は変更しない）
dgxtop --no-gpu              # NVML host を起動しない
dgxtop --read-only           # すべてのプロセス操作を無効化
dgxtop --replay fixture.json # シミュレーションデータのみ、読み取り専用、別の一時状態
dgxtop --check-config        # 設定を検証し、設定値と実効値を表示して終了
dgxtop --diagnostics-json    # バージョン、SQLite、ケイパビリティ、ヘルスを JSON で出力
dgxtop --import-legacy ~/.config/dgxtop/config.json --dry-run   # v1 → v2 の対応表を表示
dgxtop --version --verbose   # バージョン、commit、ツールチェーン、target、SQLite ランタイム
```

コマンドライン引数は明示的に指定された場合に限り、今回の実行に対してのみ保存済み設定を上書きします。

### キー操作

| キー | 動作 |
|---|---|
| `q`、`Ctrl-C` | 終了（フィルタ編集中の `q` は文字、`Ctrl-C` は常に終了） |
| `1`–`4`、`Tab`、`Shift-Tab` | Overview、GPU、History、Settings |
| `w` / `W` | History の次／前の時間範囲（5m、1h、6h、12h、24h） |
| `c` | Overview で CPU のセンサーとコアごとのバーを表示／非表示（CPU パネルのクリックでも可） |
| `j`/`k`、矢印、`PgUp`/`PgDn`、`Home`/`End` | 選択の移動（行番号ではなくプロセスの同一性で） |
| `h`/`l` | GPU ビューで前／次の GPU |
| `g` | プロセス表の GPU フィルタ切替（全 GPU → 各 GPU） |
| `/` | プロセスのフィルタ（Enter で適用、Esc でクリア） |
| `s` / `r` | ソート列／逆順 |
| `+` / `-` | サンプリング間隔の変更（ストアが書き込みを確認した後に反映） |
| `K` | 選択プロセスへの SIGTERM を要求（確認が必要） |
| `y`/`Enter`、`n`/`Esc` | 保留中のプロセス操作を確認／取消 |
| `F2`、`?`/`F1` | Settings ビュー、ヘルプ |

## 設定とファイル

| 用途 | 場所 |
|---|---|
| 設定（schema 2、リビジョン付き、アトミック書き込み） | `$XDG_CONFIG_HOME/dgxtop/v2/config.json`（`~/.config/dgxtop/v2/config.json`） |
| プロセス操作の監査（SQLite、`synchronous=FULL`） | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3`（`~/.local/state/...`） |
| 履歴アーカイブ（既定でオン、7 日間・最大 1 GiB） | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3` |
| 上限付きログ、instance lock | `$XDG_STATE_HOME/dgxtop/v2/` |

ディレクトリは `0700`、ファイルは `0600` で作成されます。完全なコマンドライン、環境変数、トークンは永続化されません。
設定の完全な契約は [`spec/dgxtop-v2/contracts/CONFIG.md`](../spec/dgxtop-v2/contracts/CONFIG.md)、既定値は
[`spec/dgxtop-v2/examples/config.default.json`](../spec/dgxtop-v2/examples/config.default.json) を参照してください。

v1 からの移行：旧 `~/.config/dgxtop/config.json` が変更されることはありません。`--import-legacy <path> --dry-run`
で対応表を確認し、`--apply` で v2 の設定ファイルを書き出します。

## アーキテクチャ

```text
データ：ドライバ／OS → adapter host（Adapter）→ Collector → Manager → Store
        Store → Manager の投影 → Viewer
操作：  入力 → Controller → Manager → 設定／スケジューラ／pidfd 操作 adapter → 監査 → Store → Viewer
```

| Crate | 役割 |
|---|---|
| `dgxtop-core` | 型付き ID、単位、品質軸、コマンド、イベント、設定、バイト予算、ポート |
| `dgxtop-runtime` | adapter-host の監督と IPC フレーミング、上限付きワーカー、ノンブロッキング端末出力 |
| `dgxtop-adapters` | 読み取り専用 adapter（procfs、hwmon、thermal、powercap、NVML、network、filesystem、replay）と独立した pidfd 操作 adapter |
| `dgxtop-collectors` | 正規化、同一ソースの重複除去、調停、カウンタレート、プロセス／GPU の関連付け |
| `dgxtop-store` | ホットスナップショット、上限付きの正確な履歴、設定リポジトリ、SQLite 監査とアーカイブ |
| `dgxtop-manager` | スケジューリング、カタログ、ポリシー、取り込み、履歴、設定、プロセス操作、投影、セッション |
| `dgxtop-ui` | 表示モジュール、レイアウト、キーマップ、コントローラ、端末ランナー |
| `dgxtop` | コンポジションルートと CLI |

`dgxtop-core` 以外のワークスペース crate に依存するのはコンポジションルートだけで、この境界は
[`spec/dgxtop-v2/tools/check_boundaries.py`](../spec/dgxtop-v2/tools/check_boundaries.py) で検査されます。

## 開発

```bash
just ci        # fmt、check、clippy -D warnings、テスト、依存境界（すべて --locked）
just e2e       # 必要なときだけ実行するエンドツーエンドテスト：PTY・障害注入・実際の host 子プロセス
just hardware  # 実機の procfs/sysfs/NVML テスト（pidfd は使い捨ての子プロセスのみ）
```

## 検証状況

各受け入れケース（AT01–AT40）のコマンド、終了コード、環境、エビデンスのハッシュは
[`docs/validation/VALIDATION.md`](validation/VALIDATION.md) を参照してください。仕様中の性能値は受け入れ目標であり、
同レポートに実測エビデンスがあるプラットフォームだけを「検証済み」と呼びます。

## ライセンス

Apache-2.0。[LICENSE](../LICENSE) を参照してください。
