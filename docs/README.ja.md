# dgxtop

<!-- ![dgxtop](screenshot.png) -->

[English](../README.md) | [繁體中文](README.zh-TW.md) | [简体中文](README.zh-CN.md)

> NVIDIA DGXシステム向けの高性能インタラクティブシステムモニター。GPU、CPU、メモリ、ディスク、ネットワークをリアルタイムで監視。

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)
[![Release](https://github.com/DennySORA/dgxtop/actions/workflows/release.yml/badge.svg)](https://github.com/DennySORA/dgxtop/releases)

**dgxtop**は、NVIDIA DGXインフラストラクチャ向けに設計された包括的なシステム監視ツールです。インタラクティブなターミナルUIを通じて、GPU使用率、VRAM、温度、消費電力、NVLinkトポロジ、システムリソースをリアルタイムで表示します。Rustで開発され、NVMLへの直接アクセスにより最高のパフォーマンスと信頼性を実現します。

## クイックインストール

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

その他のオプションは[インストール方法](#インストール方法)をご覧ください。

## なぜ dgxtop？

- **NVMLへの直接アクセス** — nvidia-smiのサブプロセス呼び出しではなく、NVIDIA Management Libraryを通じてGPUメトリクスを読み取ります。より高速、信頼性が高く、詳細な情報を取得できます。
- **DGX最適化** — マルチGPU監視、NVLinkトポロジ、ECCエラー追跡、PCIeスループット — DGX A100/H100/B200およびDGX Sparkに不可欠な機能をサポート。
- **完全なシステムビュー** — 単一のダッシュボードでCPUコアごとの使用率、メモリ（RAM + Swap）、ディスクI/O（IOPS、レイテンシ、スループット）、ネットワークインターフェースを表示。
- **インタラクティブなプロセス管理** — TUI上で直接GPUプロセスのソート、フィルタリング、終了が可能。プロセスごとのGPU使用率、VRAM使用量、ホストメモリを表示。
- **セキュアな設計** — サブプロセスシェル呼び出しなし、PIDリサイクル保護、設定値のサニタイズ、UTF-8安全レンダリング。詳細なセキュリティ監査を実施済み。

## 機能

### GPU監視（NVML経由）

| カテゴリ | メトリクス |
|----------|-----------|
| **使用率** | GPU %、メモリ %、プロセスごとのSM使用率 |
| **メモリ** | VRAM 使用/合計/空き、プロセスごとのGPUメモリ |
| **温度** | 温度、ファン回転数 |
| **電力** | 消費/上限（ワット）、使用率 |
| **クロック** | グラフィック周波数、メモリ周波数、最大周波数 |
| **健全性** | ECCエラー（修正済み/未修正）、PCIeスループット |
| **トポロジ** | NVLinkアクティブリンク、リモートGPUマッピング |

### システム監視

| カテゴリ | メトリクス |
|----------|-----------|
| **CPU** | 全体およびコアごとの使用率、ユーザー/システム/iowait内訳、温度、周波数 |
| **メモリ** | RAM使用/合計、バッファ、キャッシュ、利用可能、Swap使用量 |
| **ディスクI/O** | デバイスごとの読み書きスループット、IOPS、awaitレイテンシ、キュー深度 |
| **ネットワーク** | インターフェースごとのRX/TXスループット、パケットレート、エラー、ドロップパケット |

### インタラクティブTUI

- **3つのビュー** — 概要ダッシュボード、GPU詳細（履歴チャート付き）、フルスクリーンプロセステーブル
- **Vimキーバインド** — `j/k`でナビゲーション、`1/2/3`でタブ切替、`h/l`でGPU選択
- **プロセス管理** — GPUメモリ/使用率/CPU/PIDでソート、名前でフィルタリング、確認後に終了
- **ビジュアルデザイン** — 角丸パネル、ハーフブロック精度のグラデーションゲージ、スパークライン、交互行色、カラーコード閾値

## インストール方法

### クイックインストール（推奨）

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

### バイナリダウンロード

[GitHub Releases](https://github.com/DennySORA/dgxtop/releases)からビルド済みバイナリをダウンロード：

| プラットフォーム | アーキテクチャ | ダウンロード |
|-----------------|---------------|-------------|
| Linux | x86_64 | `dgxtop-x86_64-unknown-linux-musl.tar.gz` |
| Linux | x86_64 (glibc) | `dgxtop-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | ARM64 | `dgxtop-aarch64-unknown-linux-musl.tar.gz` |
| Linux | ARM64 (glibc) | `dgxtop-aarch64-unknown-linux-gnu.tar.gz` |

### ソースからビルド

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release
# バイナリ：target/release/dgxtop
```

### Cargo Install

```bash
cargo install --git https://github.com/DennySORA/dgxtop.git
```

## 使い方

### 基本的な使い方

```bash
# デフォルト設定で起動
dgxtop

# カスタム更新間隔（0.5秒）
dgxtop -i 0.5

# GPU監視を無効化（システムメトリクスのみ）
dgxtop --no-gpu

# 緑のカラーテーマを使用
dgxtop -t green
```

### コマンドラインオプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `-i, --interval <秒>` | 更新間隔（0.1–10.0秒） | `1.0` |
| `-t, --theme <名前>` | カラーテーマ：`cyan`、`green`、`amber` | `cyan` |
| `--no-gpu` | GPU監視を無効化 | `false` |
| `--log-level <レベル>` | ログレベル：`error`、`warn`、`info`、`debug` | `warn` |

### キーボードショートカット

| キー | アクション |
|------|-----------|
| `q` / `Ctrl+C` | 終了 |
| `Tab` / `Shift+Tab` | ビュー切替 |
| `1` / `2` / `3` | 概要 / GPU詳細 / プロセスへジャンプ |
| `j/k` または `↑/↓` | 上下ナビゲーション |
| `h/l` または `←/→` | GPU選択（GPU詳細ビュー） |
| `s` | ソートモードに入る |
| `r` | ソート順を反転（ソートモード中） |
| `/` | 名前/PID/ユーザーでプロセスをフィルタ |
| `K` | 選択したプロセスを終了（確認あり） |
| `e` | コアごとのCPU表示を切替 |
| `+` / `-` | リフレッシュ速度を上げる / 下げる |
| `?` | ヘルプの表示/非表示 |

## システム要件

- **OS**：Linux（DGXシステム、WSL2、コンテナ）
- **GPU**：NVMLを含むNVIDIAドライバー（libnvidia-ml.so）
- **実行環境**：追加の依存関係なし — 静的muslビルドを提供

## ライセンス

Apache License 2.0 — 詳細は[LICENSE](../LICENSE)をご覧ください。

## コントリビューション

コントリビューション歓迎！お気軽にIssueやPull Requestを送信してください。
