<div align="center">

# dgxtop

**NVIDIA DGX Spark のために作ったターミナル GPU モニター**

すべての GPU、それを使うプロセス、マシン全体を 1 画面で。<br>
再起動しても履歴は残ります。NVIDIA GPU を搭載したどの Linux マシンでも動きます。

[English](../README.md) · [繁體中文](README.zh-TW.md) · [プロジェクトページ](https://dennysora.me/ja/projects/tools/dgxtop/)

[![Release](https://img.shields.io/github/v/release/DennySORA/dgxtop)](https://github.com/DennySORA/dgxtop/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92.0-orange.svg)](../rust-toolchain.toml)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)

<img src="images/screenshots/dgx-spark-overview.png" alt="DGX Spark 上の dgxtop の Overview：CPU とメモリ、使用率 92% の GB10 GPU と RAM バー、ディスクとネットワークの表、GPU プロセス表" width="100%">

<sub>DGX Spark の Overview。GB10 は CPU とメモリを共有しているため、GPU プロセスが使う RAM をシステムメモリの中に描きます。</sub>

</div>

## インストール

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

x86_64 または aarch64 の Linux で、glibc 2.34 以降（Ubuntu 22.04、Debian 12、RHEL 9 以降）。DGX Spark も含みます。WSL 2 では Linux 版が動きます。GPU の値には NVIDIA ドライバーが必要で、ない場合も CPU、メモリ、ディスク、ネットワークは表示します。WSL 1、musl、macOS、Windows には対応していません。

インストーラーは CPU に合ったビルドを選び、リリースの `SHA256SUMS` で検証してから `~/.local/bin` にインストールします。以前のバージョンは残り、`./install.sh --rollback` で戻せます。特定のリリースを入れるには、`bash` の後ろに `-s -- --version v2.0.2` を付けます。

Rust 1.92.0（`rust-toolchain.toml` で固定）を使ってソースからビルドすることもできます：

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release --locked
./target/release/dgxtop
```

## 機能

htop は CPU が中心で、nvtop や nvidia-smi は見ている間だけ GPU を表示します。dgxtop は GPU を中心に、マシン全体を並べて表示し、履歴も残します。

- **すべての GPU をひと目で。** 各 GPU の使用率、メモリ、温度、電力、クロック、ファンをライブグラフ付きで表示します。
- **GPU の詳細。** 1 台ずつ表示します：
  - パフォーマンスステートとスロットルの理由、クロック
  - メモリとメモリバス、PCIe リンクと転送量、NVLink-C2C
  - 温度の上限、電力と消費電力量
  - CUDA とドライバーのバージョン
  - NUMA と CPU アフィニティ、P2P 経路、デバイスとファームウェア

  GPU が報告しない値は行を作りません。
- **GPU プロセス。** GPU を使うすべてのプロセスを、GPU、種別（計算またはグラフィックス）、GPU メモリ、CPU、RAM とともに表示します。プロセスは実行ファイル名だけでなく、実行している内容で名前を付けます：`python -m vllm.entrypoints.openai.api_server` は `vllm.api_server`、`ComfyUI` フォルダで実行した `python main.py` は `ComfyUI/main` と表示します。GPU や名前で絞り込み、任意の列で並べ替え、確認のうえでプロセスを終了できます。GPU メモリを最も多く使う 8 つのプロセスにはそれぞれ色が付き、メモリと VRAM のバーやグラフの中のそのプロセスの部分と、名前の横の印に同じ色を使います。色は名前から決まるため、同じジョブはジョブや dgxtop を再起動しても同じ色になります（実行中の別のプロセスがその色を使っている場合を除きます）。
- **履歴。** 5 分から 24 時間のグラフを 1 本の時間軸で表示します：
  - CPU、メモリ、ディスク、ネットワーク
  - 各 GPU の使用率、メモリ、温度、電力
  - すべてのグラフに現在値、平均、最大値
  - 各 GPU の直近 1・6・12・24 時間の統計と消費電力量
  - 色の付いたプロセスごとのメモリグラフの中の部分

  履歴は 7 日間ディスクに保存され、再起動しても残ります。プロセスごとの部分はメモリ上に保持し、dgxtop の実行中だけ記録します。
- **プロセス履歴（Process History）。** dgxtop が見たすべての GPU プロセスを、新しく起動した順に表示します：起動時刻と実行時間、使った GPU、GPU メモリと RAM のピーク、負荷の色で示す GPU と CPU の使用率、同時に動いた子プロセスの最大数、そして終わり方（実行中、dgxtop が停止、シグナルで終了（`SIGKILL`、`SIGTERM` など）、終了コードで終了、OOM killer の可能性大、原因が見えないまま終了）。開くと、完全なコマンドライン（`e` で全体を表示、`y` でコピー）、作業ディレクトリ、実行ファイル、親プロセス、nice 値と cgroup、起動と終了の時刻、そして Overview History と同じレイアウトで平均と最大値を添えたグラフを表示します：プロセス自身と子プロセスの CPU とメモリ、スワップ、GPU 使用率とメモリ、スレッド・子プロセス・開いているファイルの数、ディスクの読み書き、ホストのネットワーク。`w`/`W` で 1 本のバーが表す時間を選べます：1・5・10・30 分、1・2・4・6・8・12・18・24 時間、または `auto`（実行期間全体が収まる最も細かい刻み）。各サイズは最新の 300 本を保持し、古いものはより粗いサイズにまとめられているので、どれだけ長く動いても 1 プロセスは約 1 MB まで、期間で消えることもありません。64 MiB のデータベースが満ちたときだけ、終了から最も時間が経ったものから削除します。
- **マシン全体。** コアごとの CPU 使用率、ハードウェアが報告する CPU の温度と電力、メモリとスワップ、ディスクの転送量とファイルシステムの容量、ネットワークの転送量。
- **DGX Spark のために。** GB10 は CPU とメモリを共有しています。dgxtop は GPU プロセスとシステム自体（OS）がそれぞれ使うシステム RAM の量、GB10 のクラスタセンサーから得た CPU 温度、GB10 の NVLink-C2C リンクを表示します。aarch64 ネイティブビルドです。
- **アプリ内の設定。** サンプリング間隔と表示モジュールを変更できます。各設定の保存値と適用中の値、その出所、すぐ反映されるか再起動が必要かを表示します。

## 画面

### Overview

各 GPU の使用率とメモリ、CPU とメモリ、最も忙しいディスクとネットワークインターフェース、GPU を使うプロセス。`c` で CPU をコアごとのバーとセンサーに展開します。

<img src="images/screenshots/workstation-overview.png" alt="2 枚の GPU（RTX 5080 と RTX 3060）を積んだワークステーションでの Overview と、GPU メモリを使うプロセス" width="100%">

### GPU

1 台ずつ、4 本のライブバーと、ドライバーが報告するすべての情報を表示します。`h` / `l` で GPU を切り替えます。

<img src="images/screenshots/dgx-spark-gpu.png" alt="DGX Spark の GB10 の GPU ビュー：状態、クロック、ユニファイドメモリ、PCIe、NVLink-C2C、温度しきい値、電力、CUDA 13.0、ドライバー、デバイス、トポロジ" width="100%">

<img src="images/screenshots/workstation-gpu.png" alt="RTX 5080 の GPU ビュー：状態、クロック、VRAM と BAR1、PCIe Gen5、ファンとしきい値、電力上限、CUDA、ドライバー、デバイス、ファームウェア、2 枚目の GPU への P2P 経路" width="100%">

<sub>上：ユニファイドメモリと NVLink-C2C を持つ GB10。下：VRAM と PCIe Gen5 の RTX 5080。シリアル番号と UUID は伏せています。</sub>

### History

Overview の各リソースを 5 分から 24 時間（`w` / `W`）、対になったパネルで 1 本の時間軸に並べます。下には各 GPU の直近 1・6・12・24 時間の統計を表示します。

<img src="images/screenshots/dgx-spark-history.png" alt="DGX Spark の 1 時間分の履歴：CPU、メモリ、GPU 使用率、GPU メモリ、温度、電力と消費電力量、ディスク、ネットワーク" width="100%">

<img src="images/screenshots/workstation-history.png" alt="2 GPU ワークステーションの履歴と、GPU ごとの直近 1・6・12・24 時間の平均とピーク" width="100%">

### Settings

各設定の保存値と適用中の値、その出所、すぐ反映されるか再起動が必要か。表示モジュールのオン・オフも切り替えられます。

<img src="images/screenshots/dgx-spark-settings.png" alt="各設定の保存値、適用中の値、出所、再起動の要否を並べた Settings ビューと、表示モジュールの切替" width="100%">

## 特長

- **1 台のマシンに複数の GPU。** DGX Spark（GB10）と、RTX 5080 と RTX 3060 を積んだワークステーションでテスト済みです。
- **軽量。** Rust 製です。変化があったときだけ最大で毎秒 4 回描画し、データは固定の 256 MiB に収めます。
- **どの端末サイズでも。** 80×24 以上ではフルレイアウト、それ未満ではコンパクト表示です。キーボードとマウスに対応しています。
- **プライバシー。** ネットワークアクセスもテレメトリーもありません。設定と履歴は本人だけが読めるファイルです。プロセス履歴は GPU プロセスのコマンドライン、作業ディレクトリ、実行ファイルを保持します（直近 2000 プロセス）。環境変数は保存しません。
- **安全なモード。** `--read-only` でプロセス操作を無効にします。`--replay` はシミュレーションデータでデモやテストを行い、`--diagnostics-json` はバージョンと状態を出力します。
- **安全な更新。** インストーラーはダウンロードしたファイルをすべて検証し、以前のバージョンを残すので、1 コマンドで戻せます。

## アーキテクチャ

dgxtop は 6 つの構成要素からなる 1 つのプログラムです。読み取った値はデータ経路を、キー操作やコマンドは制御経路を通ります。

<img src="images/architecture/architecture-overview.png" alt="dgxtop のアーキテクチャ：procfs、hwmon、thermal、powercap、NVML、ネットワーク、ファイルシステムの adapter host が Collector に値を渡し、Manager が Store に書き込んで表示用データを Viewer に渡す。Controller とプロセス操作の adapter は制御経路にある" width="100%">

| 構成要素 | 役割 |
|---|---|
| Adapter | ハードウェアを読みます：`/proc`、hwmon、thermal zone、powercap、GPU の NVML、ネットワーク、ファイルシステム。各 adapter は専用のバックグラウンドプロセスで動きます。 |
| Collector | 読み取り値を指標に整えます：単位、重複する出所、値ごとの最適な出所、カウンターからのレート。 |
| Manager | サンプリングの計画、設定の適用、コマンドの確認を行い、データを書き込む唯一の構成要素です。 |
| Store | ライブスナップショット、履歴、設定、プロセス操作の記録を保持します。 |
| Viewer | 整えられた表示用データから、Ratatui で画面を描きます。 |
| Controller | キー、マウス操作、コマンドラインオプションをコマンドに変換します。 |

<details>
<summary>コードの構成</summary>

コードは、共通の契約 crate を中心にした 8 つの crate からなる Rust workspace です。

| Crate | 内容 |
|---|---|
| `dgxtop` | プログラム本体：起動、コマンドライン、組み立て |
| `dgxtop-core` | 共通の型：指標、単位、コマンド、インターフェース |
| `dgxtop-runtime` | バックグラウンドプロセス、IPC、worker、端末 I/O |
| `dgxtop-adapters` | procfs、sysfs、NVML、プロセス操作 |
| `dgxtop-collectors` | 正規化、出所の選択、レート |
| `dgxtop-store` | スナップショット、履歴、設定、操作の記録（SQLite） |
| `dgxtop-manager` | サンプリング、設定、コマンド、表示用データ |
| `dgxtop-ui` | 画面、ウィジェット、キーマップ |

</details>

## 使い方

```bash
dgxtop                       # ライブモニター
dgxtop -i 0.5                # この実行だけ 500 ms でサンプリング（保存済み設定は変更しない）
dgxtop --no-gpu              # NVML host を起動しない
dgxtop --read-only           # すべてのプロセス操作を無効化
dgxtop --replay fixture.json # シミュレーションデータのみ、読み取り専用、別の一時状態
dgxtop --check-config        # 設定値と有効値を検証して表示し終了
dgxtop --diagnostics-json    # バージョン、SQLite、機能、ヘルスを JSON で出力
dgxtop --import-legacy ~/.config/dgxtop/config.json --dry-run   # v1 → v2 の対応を表示
dgxtop --version --verbose   # バージョン、commit、ツールチェーン、ターゲット、SQLite ランタイム
```

| キー | 動作 |
|---|---|
| `q`、`Ctrl-C` | 終了（フィルター編集中の `q` は文字入力、`Ctrl-C` は常に終了） |
| `1`–`5`、`Tab`、`Shift-Tab` | Overview、GPU、Overview History、Process History、Settings |
| `w` / `W`、`→` / `←` | 次／前の履歴ウィンドウ（5m、1h、6h、12h、24h）。開いた実行ではバーのサイズ |
| `j`/`k`、`Enter` | Process History で実行を選んで開く／閉じる |
| `e` / `y` | 開いた実行のコマンドライン全体を表示／コピー（OSC 52） |
| `c` | CPU センサーとコアごとのバーの表示切替（CPU パネルのクリックでも可） |
| `j`/`k`、矢印、`PgUp`/`PgDn`、`Home`/`End` | 選択を移動 |
| `h`/`l`、`←`/`→` | GPU ビューで前／次の GPU |
| `g` | プロセス表の GPU フィルターを切替（全 GPU → 各 GPU） |
| `/` | プロセスをフィルター（Enter で適用、Esc でクリア） |
| `s` / `r` | ソート列／逆順 |
| `+` / `-` | サンプリング間隔を変更 |
| `K` | 選択したプロセスを終了（確認あり） |
| `F9` | 選択したプロセスを SIGKILL で強制終了（確認あり） |
| `[` / `]` | メモリパネルの GPU プロセスをスクロール |
| `y`/`Enter`、`n`/`Esc` | 保留中のプロセス操作を確認／取消 |
| `F2`、`?`/`F1` | Settings ビュー、ヘルプ |

コマンドライン引数は、その実行に限って保存済み設定を上書きします。

## 設定とファイル

| 用途 | 場所 |
|---|---|
| 設定 | `$XDG_CONFIG_HOME/dgxtop/v2/config.json`（`~/.config/dgxtop/v2/config.json`） |
| 履歴（7 日、最大 1 GiB） | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3`（`~/.local/state/...`） |
| プロセス履歴（最大 64 MiB） | `$XDG_STATE_HOME/dgxtop/v2/process_history.sqlite3` と `process_recent.json` |
| プロセス操作の記録 | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3` |
| ログとインスタンスロック | `$XDG_STATE_HOME/dgxtop/v2/` |

ディレクトリは `0700`、ファイルは `0600` で作成します。v1 からのアップグレードでは `~/.config/dgxtop/config.json` を変更しません。`--import-legacy <path> --dry-run` で対応を確認してから、`--apply` で v2 の設定ファイルを書き込みます。

## ライセンス

Apache-2.0。[LICENSE](../LICENSE) を参照してください。
