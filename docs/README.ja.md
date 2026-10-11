<div align="center">

# dgxtop

**NVIDIA DGX Spark と GPU 搭載 Linux ホストのための、誠実なターミナルモニター**

CPU · GPU · メモリ · ディスク · ネットワーク · GPU プロセス<br>
分離された収集 · 説明可能なソース · 正確な履歴 · 保護されたプロセス制御

[English](../README.md) · [繁體中文](README.zh-TW.md) · [設計ノート（英語）](https://dennysora.me/en/projects/tools/dgxtop/)

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92.0-orange.svg)](../rust-toolchain.toml)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)

<img src="images/screenshots/dgx-spark-overview.png" alt="DGX Spark 上の dgxtop の Overview：CPU とメモリ、使用率 92% の GB10 GPU と RAM バー、ディスクとネットワークの表、GPU プロセス表" width="100%">

<sub>DGX Spark の Overview。GB10 は CPU とメモリを共有しているため、GPU プロセスが使う RAM をシステムメモリの中に描きます。</sub>

</div>

dgxtop は偽の 0 を決して表示しません。読み取れなかった値は**非対応**、**読み取り失敗**、**古い**、**ソース競合**として、取得元とともに表示します。

## 画面

### Overview

CPU とメモリ、各 GPU の使用率とメモリ、最も忙しいディスクとネットワークインターフェース、そして GPU を使っているプロセス。負荷の高い時間帯はひと目で赤く見えます。`c` で CPU の全センサーとコアごとのバーを追加します。

<img src="images/screenshots/workstation-overview.png" alt="2 枚の GPU（RTX 5080 と RTX 3060）を積んだワークステーションでの Overview と、GPU メモリを使うプロセス" width="100%">

### GPU

1 台ずつ表示します。上に 4 本のリアルタイムバー、その下にドライバが報告するすべての情報。NVML と CUDA toolkit のファイルから読み取り、コマンドは実行しません。GPU が報告しない値は行ごと表示しません。

<img src="images/screenshots/dgx-spark-gpu.png" alt="DGX Spark の GB10 の GPU ビュー：状態、クロック、ユニファイドメモリ、PCIe、NVLink-C2C、温度しきい値、電力、CUDA 13.0、ドライバ、デバイス、トポロジ" width="100%">

<img src="images/screenshots/workstation-gpu.png" alt="RTX 5080 の GPU ビュー：状態、クロック、VRAM と BAR1、PCIe Gen5、ファンとしきい値、電力上限、CUDA、ドライバ、デバイス、ファームウェア、2 枚目の GPU への P2P 経路" width="100%">

<sub>上：ユニファイドメモリと NVLink-C2C の GB10。下：VRAM と PCIe Gen5 の RTX 5080。シリアル番号と UUID はモザイク処理済みです。</sub>

### History

Overview のリソースを 5 分〜24 時間の範囲で表示します（`w` / `W` で切替）。ペアのパネルが 1 本の時間軸を共有し、各ストリップにスケール、値に応じて色分けした列ごとの平均、now / avg / max を表示します。時間軸の下には GPU ごとの直近 1・6・12・24 時間の統計。履歴は `history.sqlite3` に保存され、以前の実行分もグラフに出ます。

<img src="images/screenshots/dgx-spark-history.png" alt="DGX Spark の 1 時間分の履歴：CPU、メモリ、GPU 使用率、GPU メモリ、温度、電力と消費電力量、ディスク、ネットワーク" width="100%">

<img src="images/screenshots/workstation-history.png" alt="2 GPU ワークステーションの履歴と、GPU ごとの直近 1・6・12・24 時間の平均とピーク" width="100%">

### Settings

各設定の保存値・有効値・由来と、即時反映か再起動が必要かを一覧します。表示モジュールは個別にオン／オフできます。

<img src="images/screenshots/dgx-spark-settings.png" alt="各設定の保存値、有効値、由来、再起動の要否を並べた Settings ビューと、表示モジュールの切替" width="100%">

## dgxtop の特長

- **偽の 0 を出さない。** 読み取り状態、データ種別（計測・導出・推定・シミュレーション）、品質、鮮度は別々の軸です。検証されていない単位を推測で変換したり、使用率から帯域幅を捏造したりしません。
- **ドライバが止まっても画面は止まらない。** live データソースはそれぞれ常駐 host プロセスで動きます。NVML の呼び出しがハングしても影響はその host だけで、CPU・メモリ・UI は更新を続けます。
- **説明可能なソース。** 同じセンサーだと証明された別名は 1 票として数えます。独立したソースが食い違えば、平均せずに「ソース競合」と表示します。
- **正確な履歴。** レートは正確な `u64` カウンタ差分を実経過時間で割って求め、欠損は欠損として見せます。
- **上限のあるリソース。** 管理データ全体で 256 MiB の予算を共有し、すべてのキューにバイト数と件数の上限があります。スレッド数は固定、再描画は変化時のみで最大 4 fps です。
- **保護されたプロセス制御。** `K` で選択中の GPU プロセスに SIGTERM を要求します。同一ユーザーのみ、10 秒以内の 1 回限りの確認、シグナル送信**前**の監査記録、保持済みの `pidfd` 経由での送信です。

## アーキテクチャ

### 6 つのコンポーネント、2 つの経路、1 つの書き込み役

<img src="images/architecture/architecture-overview.png" alt="アーキテクチャ：Linux カーネルと NVIDIA ドライバから 7 つの adapter host、Collector、Manager、Store、Viewer へ続くデータ経路と、Controller とプロセス操作 adapter の制御経路" width="100%">

データはカーネルとドライバから、分離された adapter host を通って Collector（正規化と調停）へ、さらに Store へ唯一書き込む Manager へ流れます。Viewer は不変の view model を描くだけで、Controller はキー入力をコマンドに変えるだけです。

### 分離されたノンブロッキング収集

<img src="images/architecture/runtime-isolation.png" alt="実行モデル：固定数のスレッドを持つ親プロセスが匿名パイプで 7 つの常駐 adapter host とつながり、ハングした NVML host は degraded、circuit open、backoff、probe、warmup の順に進む" width="100%">

タイムアウトした host はバックオフ（1 秒、2 秒、4 秒…最大 60 秒）し、回収できてから再起動します。回収できない host は隔離され、無制限に再生成されることはありません。

### 説明可能なソース調停

<img src="images/architecture/source-arbitration.png" alt="調停：hwmon が 68 °C、thermal zone が 42 °C を報告。9 つのステップの結果は選択、ソース競合、非対応、読み取り失敗、古いのいずれかで、55 °C の平均には決してならない" width="100%">

別のソースへ切り替えるには、3 回連続で勝ち、かつ 5 秒以上経過する必要があります。値がセンサー間で行き来することはありません。

### 保護されたプロセス終了

<img src="images/architecture/protected-kill.png" alt="プロセス終了：K、同一ユーザーと安全チェックの準備、pidfd_open、10 秒の確認、永続的な監査 intent、pidfd_send_signal による SIGTERM、シグナル送信済み、そして終了を確認または実行中" width="100%">

結果は「シグナル送信済み」と「終了を確認」を区別します。`SIGKILL` は `actions.allow_force` を有効にしない限り無効で、`--read-only` はすべての操作を無効にします。

### 正確な履歴

<img src="images/architecture/exact-history.png" alt="履歴：実経過時間によるカウンタ差分、分の境界をまたぐ正確な bridge、見える欠損、5 分〜24 時間のウィンドウ、history.sqlite3 アーカイブ" width="100%">

ウィンドウの合計は、ウィンドウに完全に含まれる区間だけを数えます。分をまたぐ区間は推測で分割せず、正確な bridge として保存します。

### ワークスペースの境界

<img src="images/architecture/crate-boundaries.png" alt="クレートの依存関係：dgxtop-runtime、adapters、collectors、store、manager、ui はすべて dgxtop-core だけに依存し、合成ルートの dgxtop がすべてに依存する" width="100%">

<details>
<summary>各クレートの役割</summary>

| クレート | 役割 |
|---|---|
| `dgxtop-core` | 型付き ID、単位、品質軸、コマンド、イベント、設定、バイト予算、ポート |
| `dgxtop-runtime` | adapter-host の監視と IPC フレーミング、上限付きワーカー、ノンブロッキングなターミナル出力 |
| `dgxtop-adapters` | 読み取り専用 adapter（procfs、hwmon、thermal、powercap、NVML、network、filesystem、replay）と独立した pidfd 操作 adapter |
| `dgxtop-collectors` | 正規化、同一ソースの重複排除、調停、カウンタレート、プロセスと GPU の結合 |
| `dgxtop-store` | ホットスナップショット、上限付きの正確な履歴、設定リポジトリ、SQLite の監査とアーカイブ |
| `dgxtop-manager` | スケジューリング、カタログ、ポリシー、取り込み、履歴、設定、プロセス操作、表示、セッション |
| `dgxtop-ui` | 表示モジュール、レイアウト、キーマップ、コントローラ、ターミナルランナー |
| `dgxtop` | 合成ルート（composition root）と CLI |

CI は [`spec/dgxtop-v2/tools/check_boundaries.py`](../spec/dgxtop-v2/tools/check_boundaries.py) でこの境界を検査します。

</details>

## インストール

x86_64 または aarch64 の Linux で、glibc 2.34 以降（Ubuntu 22.04、Debian 12、RHEL 9 以降）。DGX Spark も含みます。WSL 2 では Linux 版が動きます。GPU には Windows 側の NVIDIA ドライバが必要で、CPU の温度と電力は通常取得できません。WSL 1、musl、macOS、Windows には対応していません。GPU の値には NVIDIA ドライバ（NVML）が必要で、ない場合もそれ以外は動作します。

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

`install.sh` は CPU に合ったビルドを選び、`SHA256SUMS` とアーカイブ構成を検証して `dgxtop --version --verbose` を実行し、`~/.local/bin/dgxtop` をアトミックに置き換え、以前のバイナリを `dgxtop.previous` として残します（`./install.sh --rollback` で戻せます）。特定のリリースを入れるには、`bash` の後ろに `-s -- --version v2.0.0` を付けます。v2.0.0 より前のリリースは以前の v1 実装です。

ソースからビルドすることもできます（`rust-toolchain.toml` で Rust 1.92.0 に固定）：

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release --locked
./target/release/dgxtop
```

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
| `1`–`4`、`Tab`、`Shift-Tab` | Overview、GPU、History、Settings |
| `w` / `W` | 次／前の履歴ウィンドウ（5m、1h、6h、12h、24h） |
| `c` | CPU センサーとコアごとのバーの表示切替（CPU パネルのクリックでも可） |
| `j`/`k`、矢印、`PgUp`/`PgDn`、`Home`/`End` | 選択を移動（行番号ではなくプロセスの ID で追跡） |
| `h`/`l` | GPU ビューで前／次の GPU |
| `g` | プロセス表の GPU フィルターを切替（全 GPU → 各 GPU） |
| `/` | プロセスをフィルター（Enter で適用、Esc でクリア） |
| `s` / `r` | ソート列／逆順 |
| `+` / `-` | サンプリング間隔を変更（store の確認後に保存） |
| `K` | 選択プロセスに SIGTERM を要求（確認が必要） |
| `y`/`Enter`、`n`/`Esc` | 保留中のプロセス操作を確認／取消 |
| `F2`、`?`/`F1` | Settings ビュー、ヘルプ |

レイアウトは端末に合わせて変わります。80×24 以上はフルレイアウト、それ未満はコンパクトな要約、40×10 未満はサイズの案内を表示します（終了は可能）。コマンドライン引数は明示したときだけ、その実行に限って保存済み設定を上書きします。

## 設定とファイル

| 用途 | 場所 |
|---|---|
| 設定（schema 2、バージョン管理、アトミック書き込み） | `$XDG_CONFIG_HOME/dgxtop/v2/config.json`（`~/.config/dgxtop/v2/config.json`） |
| 操作の監査（SQLite、`synchronous=FULL`） | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3`（`~/.local/state/...`） |
| 履歴アーカイブ（既定で有効、7 日、最大 1 GiB） | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3` |
| 上限付きログ、インスタンスロック | `$XDG_STATE_HOME/dgxtop/v2/` |

ディレクトリは `0700`、ファイルは `0600` で作成します。完全なコマンドライン、環境変数、トークンは決して保存しません。設定の契約は [`spec/dgxtop-v2/contracts/CONFIG.md`](../spec/dgxtop-v2/contracts/CONFIG.md)、既定値は [`spec/dgxtop-v2/examples/config.default.json`](../spec/dgxtop-v2/examples/config.default.json) にあります。v1 からのアップグレードでは `~/.config/dgxtop/config.json` を変更しません。`--import-legacy <path> --dry-run` で対応を確認してから、`--apply` で v2 の設定ファイルを書き込みます。

## 開発と検証

```bash
just ci        # fmt、check、clippy -D warnings、テスト、依存境界（すべて --locked）
just e2e       # 要求時のみのエンドツーエンド：PTY セッション、障害注入、実 host プロセス
just hardware  # 実際の procfs/sysfs/NVML でのハードウェアテスト（pidfd は使い捨ての子プロセスのみ）
```

フォーマット、lint、テストスイート（x86_64 で 1,373 件、aarch64 で 1,372 件）、依存境界、ハードウェアテストは、Debian 13 のワークステーション（RTX 5080 + RTX 3060）と DGX Spark（GB10）の両方で通っています。ログと SHA-256 は [`docs/validation/evidence/`](validation/evidence/) にあります。[`docs/validation/VALIDATION.md`](validation/VALIDATION.md) は 40 件の受け入れケースを追跡しています（現在 12 PASS、24 NOT_RUN、4 BLOCKED）。3 × 30 分のベンチマークと 72 時間のソークはまだで、リリースの実行も検証の証拠として記録されていないため、検証済みと呼べるプラットフォームはなく、仕様の性能値は目標のままです。

## ドキュメント

- [設計ノート（英語）](https://dennysora.me/en/projects/tools/dgxtop/)：アーキテクチャとその背景にある考え方（[繁體中文](https://dennysora.me/zh-hant/projects/tools/dgxtop/)）。
- [`spec/dgxtop-v2/`](../spec/dgxtop-v2/README.md)：仕様（繁体字中国語）。
- [`docs/design/DECISIONS.md`](design/DECISIONS.md)：実装上の決定 D-01〜D-61。
- [`docs/design/history-redesign.md`](design/history-redesign.md)：履歴ビューの設計分析。

## ライセンス

Apache-2.0。[LICENSE](../LICENSE) を参照してください。
