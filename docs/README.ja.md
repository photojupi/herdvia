# herdvia

<p align="center">
  <a href="../README.md">English</a> · <a href="README.ko.md">한국어</a> · <strong>日本語</strong> · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <a href="https://github.com/photojupi/herdvia/actions/workflows/ci.yml"><img src="https://github.com/photojupi/herdvia/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/photojupi/herdvia/releases/latest"><img src="https://img.shields.io/github/v/release/photojupi/herdvia" alt="Latest Release"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg" alt="License: MIT OR Apache-2.0"></a>
</p>

**SSH サーバー、ポート開放（ポートフォワーディング）、VPN なしで、別々のルーター配下にある PC を [Herdr](https://herdr.dev) にリモートマシンとして接続します。**

![herdvia アーキテクチャ概要](assets/herdvia-hero.jpg)

> [!NOTE]
> **非公式・実験的プロジェクト:** `herdvia` は個人開発プロジェクトであり、Herdr / `herdr-core` 公式プロジェクトとは提携・承認・サポート関係にありません。実験段階のため、コマンド、ファイル形式、通信プロトコルは後方互換性なしに変更される場合があります。

---

## 目次

- [herdvia とは？](#what-is-herdvia)
- [仕組み](#how-it-works)
- [事前準備（必要なもの）](#requirements)
- [5分クイックスタート](#quick-start)
- [インストール方法](#install)
  - [インストールスクリプト（推奨）](#install-script)
  - [手動インストール](#manual-install)
  - [ソースコードからビルド](#from-source)
- [コマンド一覧](#commands)
- [ログイン時の自動起動](#automatic-startup)
- [トラブルシューティング](#troubleshooting)
  - [`herdvia doctor` を実行する](#run-herdvia-doctor)
  - [よくある症状](#common-symptoms)
- [アップグレードとアンインストール](#upgrading-and-uninstalling)
- [セキュリティモデル](#security-model)
- [ネットワークと公開される情報](#network-and-published-information)
- [対応プラットフォーム](#supported-platforms)
- [設定とファイルの保存場所](#configuration-and-files)
- [設計ドキュメントとライセンス](#design)

---

<a id="what-is-herdvia"></a>
## herdvia とは？

Herdr にリモート PC を追加する `hc machine add --via <コマンド>` は、通常 `ssh` のようなトランスポートを想定しています。しかし、自宅・職場・カフェなど別々のルーター（NAT）の背後にある PC 同士をつなぐには、SSH サーバーの構築、ルーターのポート開放、あるいはメッシュ VPN の導入が必要になりがちです。

**`herdvia` は、その `--via` にそのまま差し替えて使える P2P トランスポートです。** [iroh](https://www.iroh.computer)（自動 NAT 越えと公開リレーサーバーへの自動フォールバックを備えたエンドツーエンド暗号化 P2P QUIC）を利用して、2 台の PC 間で `hc` セッションのバイト列をそのまま安全に中継します。

| 接続方式 | ルーターのポート開放 | 追加デーモン / アカウント作成 | ネットワーク公開範囲 | 導入の手間 |
|---|---|---|---|---|
| **SSH (`ssh user@host`)** | NAT 越えには必要 | 接続先 PC に SSH サーバーが必要 | SSH ポートを公開 | 中〜高 |
| **メッシュ VPN** | 不要 | 仮想ネットワークドライバ + アカウント登録 | IP インターフェース全体（ACL 設定前） | 中 |
| **`herdvia`** | **不要** | **軽量な単一バイナリのみ（アカウント不要）** | **許可した公開鍵に対し `herdr-core connect` のみ実行** | **約 5 分** |

---

<a id="how-it-works"></a>
## 仕組み

`herdvia` は Herdr の内部プロトコルを解釈せず、手元の PC（**マシン A**）の `hc` と接続先 PC（**マシン B**）の `herdr-core` の間でバイト列を中継するだけに徹します。

![herdvia の動作仕組み図](assets/how-it-works.ja.svg)

1. **常駐エージェント (`herdvia agent`):** 各マシンのユーザーセッションで動作し、マシン固有の鍵（`EndpointId`）を使って許可済みピアとの暗号化 QUIC 接続を維持します。
2. **双方向の明示的な許可 (`allow` / `accept`):** 未知の端末が自動的に信頼されることはありません。発信側（マシン A）で `--outbound` を許可し、着信側（マシン B）で `--inbound` を承認して初めて通信が成立します。
3. **厳格なコマンド一致検証:** 着信側（マシン B）では、`hc` が送信する `herdr-core connect` コマンド文字列と 1 文字違わず完全一致した場合のみ実行します（**`hc 0.1.21`** で検証済み）。それ以外のコマンドはすべて拒否してログに記録します。

---

<a id="requirements"></a>
## 事前準備（必要なもの）

接続する 2 台のマシンで必要なものを確認してください。

| 要素 | マシン A（操作側 — Herdr を使用） | マシン B（接続先のリモート PC） | 備考 |
|---|---|---|---|
| **`herdr-core`** | 必須 | 必須 | 両方のマシンで**同じバージョン**を使用してください（**`0.1.21`** で検証済み） |
| **Herdr アプリ / `hc` CLI** | 必須 | 不要 | マシン B には `herdr-core` だけあれば動作します |
| **`herdvia` + エージェント起動** | 必須 | 必須 | 1 行スクリプト、リリース ZIP、または Cargo で導入 |

### 両方のマシンに `herdr-core` をインストールする

`herdvia` 自体は `herdr-core` をインストール・更新しません。両方のマシンで Herdr 公式のインストールスクリプトを実行してください（再実行すると更新されます）。

- **macOS / Linux:**
  ```sh
  curl -fsSL https://releases.herdr.dev/core/install.sh | sh
  ```
  `~/.local/bin/herdr-core`（および未作成なら `hc` リンク）を配置します。`PATH` は変更されないため、`~/.local/bin` が `PATH` になければ案内が表示されます。

- **Windows（通常の PowerShell — 管理者権限不要）:**
  ```powershell
  powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"
  ```
  `%LOCALAPPDATA%\Programs\herdr-core\herdr-core.exe`（および未作成なら `hc.exe`）をインストールし、ユーザー `PATH` に自動で追加します。

`herdvia` で動作確認済みのバージョン **`0.1.21`** を指定して入れる場合：

```sh
# macOS / Linux
curl -fsSL https://releases.herdr.dev/core/install.sh | HC_VERSION=0.1.21 sh
```

```powershell
# Windows (PowerShell)
$env:HC_VERSION = "0.1.21"; powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"
```

*（macOS では、すでにより新しい `herdr-core` が入っているとダウングレードせず停止し、`hc update --from ... --force` を使うよう案内されます。）*

> [!IMPORTANT]
> **`herdr-core` に関する重要な注意点 2 つ:**
> 1. **リモートインストール・更新コマンドの制限（`hc 0.1.21` 基準）:** `herdvia` は `hc` が送る `herdr-core connect` 行と完全一致するコマンド以外はすべて拒否するため、`hc machine add --install` や `hc machine update` でリモート側の `herdr-core` を導入・更新することはできません。また将来 `hc` 側の接続コマンドが変わった場合、`herdvia` が対応するまで拒否されます。
> 2. **マシン B で `herdr-core` を探すパス:**
>    - **macOS / Unix:** `~/.local/bin/herdr-core` またはエージェントの `PATH` 上。特に macOS で `herdvia agent install` により自動起動されたエージェントの `PATH` は `~/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin` のみのため、Homebrew 等の別パスに入れた場合は `~/.local/bin/herdr-core` にシンボリックリンクを作成してください。
>    - **Windows:** `%USERPROFILE%\AppData\Local\Programs\herdr-core\herdr-core.exe`。

---

<a id="quick-start"></a>
## 5分クイックスタート

2 台の PC を接続してみましょう：
- **マシン A** — Herdr / `hc` を操作する手元の PC（例：ノート PC）
- **マシン B** — リモート接続したい PC（例：自宅のデスクトップ）

まず **A と B の両方**に `herdvia` をインストールします（Intel Mac・Linux・手動導入は[インストール方法](#install)を参照）：

```sh
# macOS (Apple Silicon)
curl -fsSL https://github.com/photojupi/herdvia/releases/latest/download/install.sh | sh
```

```powershell
# Windows (x64) — 管理者ではなく「通常の」PowerShell ウィンドウで実行してください
powershell -c "irm https://github.com/photojupi/herdvia/releases/latest/download/install.ps1 | iex"
```

インストール後は `PATH` を反映させるため**新しいターミナルウィンドウ**を開いてください（Windows では新しい PowerShell を開き、Herdr が起動中なら再起動してください）。

ペアリング全体の流れは次の通りです：

```mermaid
sequenceDiagram
    autonumber
    participant A as マシン A (Herdr / hc)
    participant B as マシン B (リモート PC)

    Note over A,B: 手順 1〜2: 両方で herdvia 導入済み・エージェント稼働中
    B->>B: herdvia id (B の 64文字 EndpointId をコピー)
    A->>A: herdvia allow B <BのEndpointId> --outbound
    A-->>B: エージェントが B へ発信 (未許可のため拒否され pending に記録)
    B->>B: herdvia pending
    Note over A,B: A の画面に表示された 16文字の指紋 (fingerprint) を目視確認
    B->>B: herdvia accept A <Aのfingerprint> --inbound
    A-->>B: 次の再試行 (最大約30秒) で暗号化 P2P 接続が確立
    A->>B: hc machine add --via 'herdvia via B' --name B
```

### 手順 1. 各マシンの ID とフィンガープリントを確認する

**A と B の両方**で実行します（インストールスクリプトを使った場合はすでに一度表示されています）：

```sh
herdvia id
```

- **1 行目:** 64 文字の `EndpointId`（このマシンの公開鍵）
- **2 行目:** 4 文字ずつ区切った 16 文字のフィンガープリント（例：`28af-63b6-258e-859d`）。秘密鍵は初回実行時に自動生成されます。

### 手順 2. 両方のマシンでエージェントを起動する

[インストールスクリプト](#install-script)を使った場合は、エージェントの**自動起動登録と起動がすでに完了**しているため、手順 3 へ進んでください。

手動インストールした場合は、ログイン時の自動起動を登録して今すぐ起動するか（macOS・Windows）：

```sh
herdvia agent install
```

またはフォアグラウンドで直接起動します：

```sh
herdvia agent
```

OS ごとの詳細は[ログイン時の自動起動](#automatic-startup)を参照してください。

### 手順 3. マシン A でマシン B への発信を許可する (`--outbound`)

ピアが自動で信頼されることはありません。**B の `EndpointId`** を **A** にコピーし、A から B への接続を許可します：

```sh
herdvia allow B <BのEndpointId> --outbound
```

貼り付けた `EndpointId` の先頭 16 文字が、B の画面で `herdvia id` が表示したフィンガープリントと一致しているか確認してください（ID を送った経路とは別の手段や直接の画面確認で照合すると安全です）。

| 方向オプション | 意味 |
|---|---|
| `--outbound` | **このマシンから**相手ピアへ発信・接続してよい |
| `--inbound` | **相手ピアが**このマシンに着信してセッションを開いてよい |
| `--both` | 双方向とも許可する |

*名前（`A`、`B`、`home-pc` など）はローカルでの識別名です（`A-Z a-z 0-9 . _ -` の 1〜32 文字）。同じ名前で再度 `allow` を実行すると上書き更新され、`allow` は保存した項目（例: `B <EndpointId> out`）を出力します。1 つの `EndpointId` に付けられる名前は 1 つだけで、別の名前で登録済みの `EndpointId` は `allow` が拒否します。*

### 手順 4. マシン B でマシン A の着信を承認する (`--inbound`)

A のエージェントは直ちに B への接続を試みます。B の許可リストにまだ A がないため拒否されますが、64 文字の ID をもう一度コピーしなくても済むよう、B のメモリ上に試行履歴が一時記録されます。

**B** で保留リストを確認します：

```sh
herdvia pending
```

```text
fingerprint id attempts last-seen
28af-63b6-258e-859d 28af63b6258e859dc534c014248ebf7284cdbb34e7cff524e3e4d35c2df1fc55 3 4s
```

**必ず A 自身の画面に表示されているフィンガープリント**（A の `herdvia id` の 2 行目 — B の `EndpointId` を知る第三者も `pending` に現れ得るため、`pending` の表示からそのまま書き写さないでください）を読み取り、**B** で入力して承認します：

```sh
herdvia accept A <Aのfingerprint> --inbound
```

- `accept` は一致した試行の完全な `EndpointId` を（`herdvia allow A <AのEndpointId> --inbound` と同様に）登録し、`A <EndpointId> in` と出力します。
- フィンガープリントはハイフン（`-`）の有無や大文字・小文字を問いません。B から A へも発信したい場合は `--both` を指定します（`accept` では `--outbound` は指定できません）。
- A は拒否された接続を最大 **30 秒**間隔まで広げながら再試行するため、`pending` に出るまで最大約 30 秒、`accept` 後に接続されるまでさらに最大約 30 秒かかることがあります。
- *（別解：A の 64 文字 `EndpointId` を B へコピーできるなら、B で `herdvia allow A <AのEndpointId> --inbound` を実行し手順 3 と同様に照合しても構いません。）*

### 手順 5. マシン A の Herdr にマシン B を登録する

**A** で Herdr にリモートマシンを追加します：

```sh
hc machine add --via 'herdvia via B' --name B
```

- `'herdvia via B'` の `B` は手順 3 の `herdvia allow` で付けたピア名です。`--name B` は Herdr 上の表示名（別の名前でも可）です。
- `hc` はこのコマンドの末尾にリモート実行コマンドを 1 つの引数として追加して起動します。
- **`hc` が `PATH` 上の `herdvia` を見つけられない場合:** 実行ファイルのフルパスを指定し、空白を含む場合はクォートしてください。
  - **macOS:** `hc` は `--via` を `sh` コマンド行として実行するため、`sh` の規則でクォートします（インストールスクリプトがそのまま貼り付けられるコマンドを表示します）。
  - **Windows:** `hc` はシェルを介さず `--via` を分割するため（`CommandLineToArgvW` 規則）、空白を含むパスはダブルクォートで囲む必要があります。Windows PowerShell 5.1 はエスケープしない限り内側のダブルクォートを除去してしまうため、ユーザー `PATH` に `herdvia` を入れた状態にして（インストール後に新しい PowerShell ウィンドウを開き Herdr を再起動）、上記の通り `hc machine add --via 'herdvia via B' --name B` を実行する方がずっと簡単です。

### 手順 6. 接続状態を確認する

**A** で接続状態を確認します：

```sh
herdvia status
```

```text
name permission connection path rtt
B out connected direct 4.12ms
```

- **`path`:** 最初は `relay`（リレー経由）で始まり、通常は数秒以内に `direct`（直接通信）へ切り替わります。
- **macOS のローカルネットワーク権限:** 同じ Wi-Fi / LAN 内にあるのに `relay` のまま変わらない場合は、Mac の **システム設定 > プライバシーとセキュリティ > ローカルネットワーク** で `herdvia` を許可してください（[macOS の自動起動](#macos)を参照）。
- **ヒント:** B が A を `accept` する前でも、A の再試行の瞬間に A 側の `status` が一瞬 `connected` と見えることがあります。確実に確認するには **B 側**で `herdvia status` を実行してください。
- **うまく動かない場合:** 両方のマシンで `herdvia doctor` を実行してください。各 `warn` や `fail` の行に、何をすべきかがそのまま表示されます（[`herdvia doctor` を実行する](#run-herdvia-doctor)を参照）。

---

<a id="install"></a>
## インストール方法

<a id="install-script"></a>
### インストールスクリプト（推奨）

**macOS (Apple Silicon):**

```sh
curl -fsSL https://github.com/photojupi/herdvia/releases/latest/download/install.sh | sh
```

**Windows (x64)** — 管理者ではなく**通常の PowerShell ウィンドウ**で実行：

```powershell
powershell -c "irm https://github.com/photojupi/herdvia/releases/latest/download/install.ps1 | iex"
```

スクリプトが行う 6 つの処理：

1. **ダウンロードと検証:** 一時フォルダにリリースアーカイブと `SHA256SUMS` を取得し、SHA-256 チェックサムを検証します。失敗した場合は何も変更せず終了します。
2. **バイナリの配置:** macOS は `~/.local/bin/herdvia`、Windows は `%LOCALAPPDATA%\Programs\herdvia\herdvia.exe`（README・ライセンス類と同梱）に配置します。
3. **`PATH` の設定:** macOS ではログインシェルが `zsh` の場合に `~/.zprofile` へ `export PATH="$HOME/.local/bin:$PATH"` を追記します（他のシェルの場合は追記すべき行を表示）。Windows ではユーザー `PATH` に追加します。
4. **`herdr-core` の確認:** **`herdr-core` 自体はインストールしません。** 未導入なら公式コマンドを表示し、macOS で `~/.local/bin/herdr-core` 以外の場所にのみある場合は `ln -s` コマンドを案内します。
5. **ID の生成と表示:** `herdvia id` を実行して `secret.key`（必要な場合）を初期化し、`EndpointId` とフィンガープリントを表示します。
6. **エージェントの自動起動登録:** `herdvia agent install` を実行し、今すぐ起動するとともに次回ログイン時からの自動起動を登録します（Windows の管理者 PowerShell から実行された場合はスキップして通常ウィンドウでの実行を案内します）。

**スクリプトの動作条件:**
- **macOS:** Apple Silicon 専用（`root` 実行は拒否。Intel Mac や Linux では何も変更せず[ソースコードからビルド](#from-source)を案内）。
- **Windows:** x64 専用、`System32\curl.exe` が必要（Windows 10 1803 以降に標準搭載）。

**環境変数オプション（スイッチ類は値が `1` のときのみ有効）:**

| 変数名 | 効果 |
|---|---|
| `HERDVIA_VERSION` | 最新版ではなく指定バージョン（例：`0.1.0` または `v0.1.0`）をインストール |
| `HERDVIA_NO_AGENT=1` | `herdvia agent install` を実行しない |
| `HERDVIA_NO_MODIFY_PATH=1` | `~/.zprofile` や Windows のユーザー `PATH` を変更しない |
| `HERDVIA_RELEASE_BASE` | GitHub Releases の代わりに `<url>/<ファイル名>` から取得（`HERDVIA_VERSION` より優先） |

バージョンを固定してインストールする場合（バージョン付き URL のスクリプトを取得する場合でも `HERDVIA_VERSION` の指定が必要です）：

```sh
curl -fsSL https://github.com/photojupi/herdvia/releases/download/v0.1.0/install.sh | HERDVIA_VERSION=0.1.0 sh
```

```powershell
$env:HERDVIA_VERSION = "0.1.0"; powershell -c "irm https://github.com/photojupi/herdvia/releases/download/v0.1.0/install.ps1 | iex"
```

実行前にスクリプトの中身を確認したい場合（`SHA256SUMS` にはスクリプトのハッシュも含まれます）：

```sh
# macOS
curl -fsSLO https://github.com/photojupi/herdvia/releases/latest/download/install.sh
less install.sh
sh install.sh
```

```powershell
# Windows (PowerShell)
curl.exe -fLO https://github.com/photojupi/herdvia/releases/latest/download/install.ps1
notepad install.ps1
Get-Content -Raw .\install.ps1 | Invoke-Expression
```

*（スクリプトを再実行するとアップグレードになります。[アップグレード](#upgrading)を参照してください。）*

<a id="manual-install"></a>
### 手動インストール

[GitHub Releases](https://github.com/photojupi/herdvia/releases) からダウンロードします：

- `herdvia-macos-arm64.zip`（macOS Apple Silicon）
- `herdvia-windows-x64.zip`（Windows x64）
- `install.sh` および `install.ps1`
- `SHA256SUMS`（アーカイブおよびスクリプトの SHA-256 チェックサム）

各アーカイブには親フォルダなしで実行ファイル、`README.md`、`LICENSE-MIT`、`LICENSE-APACHE`、`THIRD-PARTY-LICENSES.html` が入っています。自動起動登録では実行時のファイルパスがそのまま記録されるため、ダウンロードフォルダ等に残さず固定の場所（macOS は `~/.local/bin/herdvia`、Windows は `%LOCALAPPDATA%\Programs\herdvia\herdvia.exe`）に配置してください。

バイナリには署名・公証（notarization）はありません。Intel Mac 用のビルド済みバイナリはないため、[ソースコードからビルド](#from-source)してください。

`v0.1.2` より後に公開されたリリースファイルには、GitHub のビルド来歴証明（build provenance attestation）が付きます。[GitHub CLI](https://cli.github.com) で `gh attestation verify <ファイル> --repo photojupi/herdvia`（例: `gh attestation verify herdvia-macos-arm64.zip --repo photojupi/herdvia`）を実行すると、そのファイルがこのリポジトリのリリースワークフローでビルドされたことを確認できます。これはファイルがどこでビルドされたかを示すだけで、安全であることの保証ではありません。メンテナーのアカウントを乗っ取った者が作成したタグからビルドされたリリースも検証を通ります。`SHA256SUMS` は同じリリースから取得するため、ダウンロード中の破損は検出できますが、リリース自体の差し替えは検出できません。

<a id="macos-apple-silicon"></a>
#### macOS (Apple Silicon)

```sh
mkdir -p ~/.local/bin
cd "$(mktemp -d)"
curl -fLO https://github.com/photojupi/herdvia/releases/latest/download/herdvia-macos-arm64.zip &&
curl -fLO https://github.com/photojupi/herdvia/releases/latest/download/SHA256SUMS &&
shasum -a 256 -c --ignore-missing SHA256SUMS &&
unzip -q herdvia-macos-arm64.zip -d herdvia &&
mv herdvia/herdvia ~/.local/bin/herdvia
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile
```

- `~/.local/bin` がすでに `PATH` に含まれている場合は最後の `echo ... >> ~/.zprofile` を省略してください。
- ダウンロードフォルダへのシンボリックリンクではなく、実行ファイル本体を `~/.local/bin` に移動してください（`launchd` はシンボリックリンクのパスを記録するため、リンク先が削除されると起動できなくなります）。
- 新しいターミナルを開いて `herdvia id` で動作確認します。
- ブラウザで ZIP をダウンロードした場合、Gatekeeper の隔離属性が付与されることがあります。以下のコマンドで解除できます：
  ```sh
  xattr -d com.apple.quarantine ~/.local/bin/herdvia
  ```

<a id="windows-x64"></a>
#### Windows (x64)

管理者ではない**通常の PowerShell ウィンドウ**で 1 つずつ確認しながら実行してください（エラーが出たらそこで中断します）：

```powershell
# 1. 一時フォルダを作成して移動
$tmp = Join-Path $env:TEMP "herdvia-$(Get-Random)"
New-Item -ItemType Directory $tmp
Set-Location $tmp

# 2. アーカイブと SHA256SUMS をダウンロード
curl.exe -fLO https://github.com/photojupi/herdvia/releases/latest/download/herdvia-windows-x64.zip
curl.exe -fLO https://github.com/photojupi/herdvia/releases/latest/download/SHA256SUMS

# 3. SHA-256 ハッシュを照合（必ず一致していることを確認！）
certutil -hashfile herdvia-windows-x64.zip SHA256
Select-String herdvia-windows-x64.zip SHA256SUMS

# 4. %LOCALAPPDATA%\Programs\herdvia に展開（ハッシュが一致した場合のみ）
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\Programs\herdvia"
tar.exe -xf herdvia-windows-x64.zip -C "$env:LOCALAPPDATA\Programs\herdvia"
```

`herdvia` を名前だけで実行できるよう、`%LOCALAPPDATA%\Programs\herdvia` を**ユーザー環境変数 `Path`**（*スタートメニュー → 「アカウントの環境変数を編集」 → ユーザー環境変数 → `Path` → 編集 → 新規*）に追加してから、新しい PowerShell ウィンドウを開き（Herdr が起動中なら再起動）、`herdvia id` で確認してください。初回実行時に Windows SmartScreen の確認画面が表示されることがあります。

<a id="from-source"></a>
### ソースコードからビルド

`iroh` の要件により **Rust 1.91 以降**が必要です。

- **macOS:** Xcode Command Line Tools（`xcode-select --install`）と [rustup](https://rustup.rs) による Rust
- **Windows:** [rustup](https://rustup.rs) と Visual Studio Build Tools（「C++ によるデスクトップ開発」）。*（注：公式リリースバイナリは Linux 上で MinGW `x86_64-pc-windows-gnu` によりクロスコンパイルされており、CI では MSVC で単体テストのみを実行します。）*

```sh
cargo install --locked --git https://github.com/photojupi/herdvia
```

デフォルトで `main` ブランチをビルドし、`~/.cargo/bin`（Windows は `%USERPROFILE%\.cargo\bin`）に配置します。特定のリリースをビルドするには `--tag v<バージョン>` を追加してください。

---

<a id="commands"></a>
## コマンド一覧

| コマンド | 説明 |
|---|---|
| `herdvia id` | このマシンの `EndpointId`（1 行目）と 16 文字のフィンガープリント（2 行目）を表示します。`secret.key` がなければ生成します。 |
| `herdvia allow <名前> <EndpointId> (--inbound \| --outbound \| --both)` | ピアを追加または更新し、保存した項目（`<名前> <EndpointId> <in\|out\|both>`）を出力します。別の名前で登録済みの `EndpointId` は拒否します。稼働中のエージェントには即座に反映されます（または次回エージェント起動時に反映）。 |
| `herdvia revoke <名前>` | ピアを削除して `revoked <名前> <EndpointId>` を出力し、そのピアとの接続を即座に切断して、その接続が起動した `herdr-core connect` プロセスを終了させます（これで元に戻らないものは[セキュリティモデル](#security-model)を参照）。その名前のピアがなければ失敗します。同じ `EndpointId` が別の名前でまだ登録されている場合（以前のバージョンが書いた `peers` ファイル）は警告します。 |
| `herdvia status` | 各ピアの名前、権限、接続状態、経路（`direct`/`relay`）、往復遅延（RTT）を表示します。エージェントの起動が必要です。 |
| `herdvia pending` | 着信接続の試行が拒否されたマシンを一覧表示します（フィンガープリント、`EndpointId`、試行回数、最終確認からの経過時間）。メモリ上のみ最大 16 件・最終試行から 10 分間保持。エージェントの起動が必要です。 |
| `herdvia accept <名前> <fingerprint> (--inbound \| --both)` | `pending` 内で指定フィンガープリントに一致する 1 台を完全な `EndpointId` で登録します。一致が 0 件または複数件の場合、リストが満杯の場合（16 件。誰かが接続試行を大量に送っている可能性があるため、完全な `EndpointId` で `allow` を使ってください）、`<名前>` が別の ID で登録済みの場合、またはその `EndpointId` が別の名前で登録済みの場合は拒否します。エージェントの起動が必要です。 |
| `herdvia doctor` | 何も変更せずにこのマシンの設定状態（実行ファイル、データフォルダ、`secret.key`、`peers`、`herdr-core`、自動起動、エージェント、リレー、ピア接続、保留中の着信試行）を点検します。`ok`/`warn`/`fail` の行を出力し（ピア接続はピアごとに 1 行、前の段階が失敗した場合は後続の点検を省略）、1 つでも失敗があれば終了コード `1` で終了します。[`herdvia doctor` を実行する](#run-herdvia-doctor)を参照。 |
| `herdvia --version` | `herdvia <バージョン>` を表示します。 |
| `herdvia agent` | エージェントをフォアグラウンドで実行します（`Ctrl-C` で停止）。 |
| `herdvia agent install` | ログイン時の自動起動を登録し、今すぐエージェントを起動します（macOS、Windows）。 |
| `herdvia agent uninstall` | 自動起動エージェントを停止し、登録を解除します。データフォルダとログは残ります。 |
| `herdvia via <ピア> <行>` | `hc` から内部的に呼び出されるサブコマンドです（手動実行用ではありません）。ローカルエージェントの起動が必要です。 |

エラー時、`herdvia` は標準エラー出力（`stderr`）に `herdvia:` で始まる 1 行を出力し、終了コード **`255`** で終了します。

---

<a id="automatic-startup"></a>
## ログイン時の自動起動

<a id="macos"></a>
### macOS

`herdvia agent install` はユーザー単位の LaunchAgent（`~/Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist`）を作成し、`launchctl bootstrap` で起動します。ログイン時に自動起動し、終了しても `launchd` が再起動します。ログは `~/Library/Logs/herdvia.log` に出力され、エージェントが 180 日を過ぎた行を削除し、ファイルが 10 MiB を超えると切り詰めます（[設定とファイルの保存場所](#configuration-and-files)を参照）。

- `HERDVIA_DIR` が設定されている場合（自動起動は常にデフォルトのデータフォルダを使用します）や、フォアグラウンドのエージェントが動作中の場合はインストールを拒否します。
- 再度 `herdvia agent install` を実行すると、現在の実行ファイルパスで登録を更新しエージェントを再起動します。
- 一時フォルダや Cargo の `target` ディレクトリから実行した場合は警告を表示しつつ登録します。

> [!IMPORTANT]
> **macOS 15 以降のローカルネットワーク権限:** macOS 15（Sequoia）以降では、自動起動されたエージェントが同一 LAN 上のアドレスへ直接通信するには許可が必要です。macOS の確認ダイアログで**許可**を選ぶか、**システム設定 > プライバシーとセキュリティ > ローカルネットワーク** で `herdvia` をオンにしてください。許可するまでは同一 LAN 内のピアとも `direct` ではなく `relay` 経由になります（別ネットワークのピアや、ターミナルから `herdvia agent` で手動起動した場合は影響ありません）。

### Windows

`herdvia agent install` は現在のユーザーの `Run` レジストリキー（`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`）に `conhost.exe --headless "<herdvia.exe のパス>" agent --log` を登録し、コンソールウィンドウなしで即座に起動します。管理者権限、サービス、タスクスケジューラは一切使用しません。

- **必ず通常の（管理者ではない）PowerShell ウィンドウから実行してください。** 管理者 PowerShell では拒否され、エージェント自体も「管理者として実行」したウィンドウからは起動を拒否します（[セキュリティモデル](#security-model)を参照）。
- `HERDVIA_DIR` が設定されている場合や、自動起動未登録の状態でエージェントが動作中の場合は拒否します（自動起動登録済みなら、既存のデフォルトフォルダのエージェントを停止して新しい方を起動します）。
- 実行ファイルのパスに `%` を含めることはできず、登録コマンドライン全体が 259 UTF-16 コード単位以内に収まる必要があります。
- ログは `%LOCALAPPDATA%\herdvia\herdvia.log` に出力されます（180 日を過ぎた行は削除され、ファイルが 10 MiB を超えると切り詰められます）。エージェントが途中で終了した場合、Windows は自動再起動せず次回サインイン時に起動します。

### Linux

自動起動（`agent install` / `agent uninstall`）には対応していません。`herdvia agent` をご自身で起動してください（例：`systemd --user` 配下またはセッションマネージャー内）。

---

<a id="troubleshooting"></a>
## トラブルシューティング

<a id="run-herdvia-doctor"></a>
### `herdvia doctor` を実行する

`herdvia doctor` は以下の項目を順番に点検します：

1. **この実行ファイル:** バージョンとパス（`agent install` がそのパスを記録するため、一時フォルダや Cargo の `target` ディレクトリから実行されている場合は警告）。
2. **データフォルダ:** パス（`HERDVIA_DIR` 由来かどうかも含む。`HERDVIA_DIR` は絶対パスである必要あり）、存在するか、ディレクトリであるか（シンボリックリンク不可、Windows ではジャンクションも不可）、そしてこのユーザーが開けるか（macOS/Unix では所有者も確認し、開けない場合は `fail` となり macOS/Unix では `chmod 700` を案内）。macOS ではフォルダにアクセスを許可する ACL（アクセス制御リスト）項目があっても `fail` となり `chmod -N` を案内し、Windows では他の種類の再解析ポイント（reparse point）である場合や、所有者がこのユーザー・Administrators・SYSTEM 以外の場合に `fail` となります。macOS/Unix ではエージェントのソケットパスが Unix ソケット長の上限に収まるかも確認します。
3. **`secret.key`:** 存在するか、およびエージェントと同じ検査に通るか（macOS/Unix: このユーザーの所有で権限 `0600` 以下、macOS ではアクセスを許可する ACL 項目がないこと。Windows: リンクではない通常のファイルで、所有者がこのユーザー・Administrators・SYSTEM のいずれか）。出力にはフィンガープリントのみが表示されます。
4. **`peers`:** 読み取れるか（`secret.key` と同じ検査）・解析できるか、および方向ごとの登録ピア数。
5. **`herdr-core`:** エージェントが探す場所（macOS では自動起動エージェントの `PATH`）にあるか、また `herdr-core --version` が検証済みの `0.1.21` を返すか。
6. **自動起動:** 登録されているか（macOS の LaunchAgent plist、Windows の `Run` レジストリ値）、かつこの実行ファイルを指しているか。Windows で管理者として実行されている場合も警告します。
7. **エージェント:** このデータフォルダのエージェントが応答するか、そのバージョンと実行ファイル、およびホームリレーに接続されているか。
8. **ピア:** `herdvia status` に基づき、登録されたピアごとに接続状態と経路を 1 行ずつ表示（ピアが未登録の場合は `warn` を 1 行表示）。
9. **保留中の試行（`pending`）:** `herdvia pending` に表示される拒否された着信試行の件数。

出力行は標準出力（`stdout`）に `ok`、`warn`、`fail` のいずれかで始まり、`warn` と `fail` の行には対処法が同じ行に書かれます。ピア接続はピアごとに 1 行出力され、前の段階が失敗した場合はそれに依存する後続の点検が省略されます：データフォルダの点検が `fail` になるとエージェント点検とそれ以降の行はすべて省略され（Unix ソケットパス長の上限超過のみが原因である場合を除き、`secret.key` と `peers` も省略）、エージェントが応答しない場合は `relay`、ピア、`pending` の行が省略されます。失敗（`fail`）が 1 つもなければ終了コード `0`（警告のみでも `0`）、1 つでも失敗があれば `1` で終了します。引数エラーなどは他のコマンドと同様に `herdvia:` の 1 行を出力して `255` で終了します。

`doctor` は読み取り専用であり、データフォルダ、`secret.key`、`peers`、自動起動の登録、エージェントの状態を作成・変更しません。実行するプログラムは `herdr-core --version`（2 秒制限）のみです。エージェントへの各リクエストには 3 秒の制限があり、エージェントが起動中でも 3 秒以内に応答しない場合は `fail agent: it may be running, but it did not answer within 3 seconds; …` を出力して残りのエージェント点検を省略します。続くピア状態や `pending` のリクエストが 3 秒を超えた場合はその行が `warn` になります（`status` がタイムアウトした場合は `pending` を省略）。`herdvia doctor` は `v0.1.2` で追加されました。それより前のバージョンのエージェントは `doctor` リクエストを知らないため、`doctor` はエージェントがこの実行ファイルより古く再起動が必要であると警告し（`relay` は省略しつつピアと `pending` は点検します）、エージェント側のログには `unknown CONTROL request` が 1 行記録されます。

**両方のマシン**で実行してください。B 側の問題（`herdr-core` の未導入や未承認のピアなど）は B の出力に現れます。マシン A での実行例：

```text
$ herdvia doctor
ok   executable: herdvia 0.1.2 at /Users/you/.local/bin/herdvia
ok   data directory: /Users/you/Library/Application Support/herdvia
ok   secret.key: present, fingerprint 28af-63b6-258e-859d
ok   peers: 2 registered (in 0, out 1, both 1)
ok   herdr-core: /Users/you/.local/bin/herdr-core (hc 0.1.21)
ok   automatic start: registered for this executable
ok   agent: running herdvia 0.1.2 from /Users/you/.local/bin/herdvia
ok   relay: connected to https://aps1-1.relay.n0.iroh.link./
ok   peer B (out): connected directly, 4.12ms
warn peer office (both): not connected; its agent is off, or it has not allowed this PC (on that PC: herdvia pending, then herdvia accept)
ok   pending: no rejected connection attempts
```

<a id="common-symptoms"></a>
### よくある症状

| 症状 | 主な原因 | 対処法 |
|---|---|---|
| B で `herdvia pending` を実行しても空のまま | A のエージェント未起動、A で `--outbound` 未設定、または再試行の待機中 | A で `herdvia status` が動くか、`herdvia allow B <B_ID> --outbound` 済みかを確認し、最大 30 秒待ってから再度 `herdvia pending` を実行 |
| 同じ Wi-Fi 内なのに `status` がずっと `relay` のまま | macOS 15+ で LaunchAgent のローカルネットワークアクセスが未許可 | Mac の **システム設定 > プライバシーとセキュリティ > ローカルネットワーク** で `herdvia` をオンにする |
| Homebrew で `herdr-core` を入れた Mac で Herdr に `herdr-core is not installed here` と表示される | macOS の LaunchAgent は `~/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin` のみを検索する | `mkdir -p ~/.local/bin && ln -s "$(command -v herdr-core)" ~/.local/bin/herdr-core` を実行 |
| `hc machine add --install` や `hc machine update` が失敗する | `herdvia` は `herdr-core connect` 以外のコマンドをすべて拒否する仕様 | 各マシンで直接 Herdr 公式の `herdr-core` インストールスクリプトを実行する |
| インストール直後に `herdvia` が見つからないと言われる | 開いているターミナルや Herdr アプリに新しい `PATH` がまだ反映されていない | 新しいターミナルを開き、Windows では起動中の Herdr アプリも再起動する |
| Windows で `herdvia agent install` や `herdvia agent` が管理者エラーになる | 管理者として実行した PowerShell でコマンドを打っている | 通常の（非管理者）PowerShell を開いて `herdvia agent install` を実行する |
| B が承認する前なのに A の `status` が一瞬 `connected` になる | ハンドシェイク完了後に B が切断するまでのわずかな間だけ表示される | 本当に接続確立したかは着信側の **B** で `herdvia status` を確認する |

---

<a id="upgrading-and-uninstalling"></a>
## アップグレードとアンインストール

マシンの秘密鍵（`secret.key`）とピアリスト（`peers`）は実行ファイルの隣ではなく専用のデータフォルダに保存されているため、実行ファイルを置き換えたり削除したりしても ID やペアリング設定はそのまま維持されます。現在のバージョンは `herdvia --version` で確認できます。

<a id="upgrading"></a>
### アップグレード

**推奨 — [インストールスクリプト](#install-script)を再実行する:**
新しいバイナリを取得・検証して安全に置き換え（macOS はアトミックな `mv`、Windows は稼働中ロックを回避するため `herdvia.new.exe` で動作確認後に `.old` へリネーム置換）、`herdvia agent install` を再実行して新しいバイナリでエージェントを再起動します。`PATH` が重複追加されることはありません。

*（手動起動したフォアグラウンドのエージェントが動作中、または `HERDVIA_DIR` が設定されている場合は、そのエージェントを止めるか `HERDVIA_DIR` を解除して `herdvia agent install` を実行するか、`HERDVIA_NO_AGENT=1` を渡してご自身でエージェントを再起動してください。）*

**手動アップグレード:**
- **macOS:** [macOS (Apple Silicon)](#macos-apple-silicon) のダウンロード・検証・`mv` 手順を再実行します（`.zprofile` の行は不要）。**稼働中のバイナリを `cp` で上書きしないでください**（次回起動時に `Killed: 9` になります）。必ず `mv` で置き換えてから `herdvia agent install` を実行してください（またはフォアグラウンドの `herdvia agent` を再起動してください）。
- **Windows:** 実行中の `.exe` は `tar.exe` で上書きできないため、先に `herdvia agent uninstall`（フォアグラウンドなら `Ctrl-C`）で停止してから [Windows (x64)](#windows-x64) の手順で展開し、再度 `herdvia agent install` を実行します。

アップグレード後、稼働中のエージェントが古い実行ファイルやバージョンのままになっている場合、`herdvia doctor` が警告します。

### アンインストール

1. **エージェントの停止と自動起動解除:** `herdvia agent uninstall` を実行（フォアグラウンド実行中なら `Ctrl-C`）。
2. **実行ファイルの削除:**
   - macOS: `rm ~/.local/bin/herdvia`
   - Windows (PowerShell): `Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\herdvia"`
3. **鍵・ピアリスト・ログの削除（任意）:** [設定とファイルの保存場所](#configuration-and-files)を参照し、データフォルダとログファイルを削除します。
4. **`PATH` の整理:**
   - Windows: ユーザー環境変数 `Path` から `%LOCALAPPDATA%\Programs\herdvia` を削除します。
   - macOS: `~/.zprofile` の `~/.local/bin` 行は `hc` や `herdr-core` 等も共有しているため、他に使うツールがない場合のみ削除してください。
5. **相手マシン側の登録解除:** このマシンを許可していた他のマシンで `herdvia revoke <名前>`、Herdr を動かしていたマシンで `hc machine rm <名前>` を実行します。

---

<a id="security-model"></a>
## セキュリティモデル

> [!WARNING]
> **`--inbound`（または `--both`）で許可したピアは、このマシン上であなたのユーザー権限として `herdr-core` セッションを開けます — 実質的にシェル権限を与えるのと同等です。** 自分自身の端末と同等に信頼できる機器のみを許可してください。

- **公開鍵による相互認証:** すべての接続は `iroh` により各マシンの Ed25519 鍵ペア（`EndpointId`）で認証されます。ピアリストが空、または方向権限（`in`、`out`、`both`）が一致しない接続はすべて拒否されます。権限はストリームごとに再確認され、`revoke` / `allow` の変更は稼働中のエージェントに即時反映されます（エージェントの起動中に行った変更も含む）。
- **`revoke` で元に戻らないもの:** `revoke` はそのピアとの経路を切断し、経路が起動した `herdr-core connect` プロセスを終了させます。そのピアがすでに開いたリモートターミナルは `herdr-core` の中に残り、すでに起動したプロセスや変更した内容もそのまま残ります。ピアの侵害が疑われる場合は revoke したうえで、この PC の Herdr セッションと、エージェントログの `session started` / `session ended` の行を確認してください（[設定とファイルの保存場所](#configuration-and-files)を参照）。
- **inbound の信頼は連鎖します:** ピアに `--inbound` を与えた PC の安全性は、そのピアの安全性を超えません。そのピアを掌握した者は、この PC でもセッションを開けるからです。`--outbound` は Herdr を使うマシンにだけ、`--inbound` はそのマシンが接続する先のマシンにだけ与え、`--both` は両方向で実際に接続し合う場合にだけ使ってください。
- **outbound で接続する相手もバイトを送ってきます:** この PC が接続するピアは、この PC の `hc` が読むバイト、エラー文言、終了コードを送ってきます。信頼できる PC にだけ接続してください。
- **コマンド許可リストの位置づけ:** `hc` の正確な `herdr-core connect` 行のみを通し他を拒否・ログ記録する仕組みは、誤用を減らし異常を検知するためのものです。`herdr-core connect` 自体が任意のコマンドを実行できるため、**本来のセキュリティ境界はピアの鍵リスト（`peers`）です。**
- **`root`・管理者権限での実行の拒否:** macOS/Unix では `root` での起動を拒否します。Windows では、`herdvia agent` は「管理者として実行」したターミナル（UAC で分割されたトークンの昇格側）からの起動を拒否し、`herdvia agent install` は昇格したトークンをすべて拒否します。UAC を無効にした PC や組み込みの Administrator アカウントでは、エージェントは起動します。
- **接続の上限:** 接続済みのピアが 1 つの接続で同時に開けるストリームは最大 16 本（`hc` セッションごとに 1 本）で、単方向ストリームとデータグラムは使えません。ストリームを開いてから 10 秒以内に最初の要求を送らなければ、そのストリームは閉じられます。
- **ユーザー分離されたローカル IPC:**
  - **macOS / Unix:** `0700` データフォルダ内の `0600` ソケットを使用し、接続元の UID を検証します。
  - **Windows:** DACL により現在のユーザー SID のみに制限された名前付きパイプを使用し、リモートネットワークからのパイプクライアントを拒否します。エージェントはデータフォルダの `agent.lock` をロックしているため、同じフォルダで 2 つ目のエージェントを起動すると拒否されます。別のアカウントがすでにパイプ名を占有している場合は、そのアカウントの SID を示して起動を拒否します。
- **データフォルダ、`secret.key`、`peers`:** macOS/Unix ではデータフォルダを `0700` にし、`secret.key` を権限 `0600` で作成します。`secret.key` や `peers` が他のユーザーの所有であるか権限がより広い場合はエラーで停止し、macOS ではフォルダや 2 つのファイルにアクセスを許可する ACL（アクセス制御リスト）項目がある場合も停止します（エラー文言に ACL を削除する `chmod -N` コマンドが表示されます）。Windows では、データフォルダがジャンクションなどの再解析ポイントである場合や、所有者がこのユーザー・Administrators・SYSTEM 以外の場合は拒否し、この 3 者だけにアクセスを与える保護された DACL をフォルダに設定して中のファイルに継承させます。リンクであるか所有者が異なる `secret.key`・`peers` も拒否します。
- **`secret.key` はこの PC の ID です:** このファイルを持つ者は誰でも、この PC を信頼しているピアに対してこの PC になりすませます。漏洩した可能性がある場合や、バックアップから復元した場合、別の PC に移した場合は、鍵を交換してください：
  1. すべてのピアで、この PC の項目を `herdvia revoke <名前>` で削除します。
  2. エージェントを停止します（`herdvia agent uninstall`、フォアグラウンド実行中なら `Ctrl-C`）。
  3. `secret.key` を削除します。
  4. `herdvia id` を実行して新しい ID を作成します。
  5. `herdvia agent install` を実行します（または `herdvia agent` を起動し直します）。
  6. すべてのピアと改めてペアリングします。
- **フィンガープリントと `pending` / `accept`:** フィンガープリントは人間が照合するために 256 ビット `EndpointId` の先頭 16 桁の 16 進文字（64 ビット）のみを表したもので、`herdvia` が保存・検証するのは常に完全な `EndpointId` です。`pending` は認証済み `iroh` ハンドシェイクを完了した試行のみをメモリ上に最大 16 件・10 分間保持し、`accept` は一致がただ 1 件のときのみ登録します。リストはこの PC の `EndpointId` を知る誰もが埋められるため、満杯のときは登録しません。フィンガープリントは `pending` 自体からではなく、必ず**相手マシン自身の画面**から読み取ってください。

**脆弱性の報告:** 機密性の高いセキュリティ上の問題は、GitHub の[非公開脆弱性報告（Private vulnerability reporting）](https://github.com/photojupi/herdvia/security/advisories/new)からご連絡いただき、機密性のないバグについては通常の Issue を作成してください。

---

<a id="network-and-published-information"></a>
## ネットワークと公開される情報

`herdvia` は `iroh` のデフォルト `N0` プリセットを使用します（変更オプションはありません）：

- **公開リレーサーバー（[n0](https://n0.computer)）:** 各エージェントはホームリレーに接続し、接続確立の仲介や、直接通信できない場合のフォールバック経路として使用します。
- **DNS アドレス検索（`dns.iroh.link`）:** `EndpointId` から互いを見つけられるよう、各エージェントは自身の **`EndpointId` とホームリレー URL** を含む署名付きレコードを公開サービスへ登録します。
- **ルーターのポートマッピング:** ルーターが対応していれば UPnP、PCP、NAT-PMP によるポート開放を要求し、直接接続を補助します。
- **IP アドレスと `EndpointId`:** アドレス検索サービスに登録されるのはホームリレー URL だけですが、この PC の `EndpointId` を知る者はリレー経由でこの PC に接続でき、`herdvia` がその接続を拒否する前に、`iroh` がこの PC の直接接続用アドレス候補（LAN アドレスやポートマッピングされたグローバルアドレスを含む）をその相手に送ることがあります。そのため、この PC の現在の IP アドレスを知られる可能性があります。`EndpointId` は秘密ではありませんが、公開の場には掲載しないでください。
- **暗号化とメタデータ:** セッション通信はすべて QUIC/TLS によりエンドツーエンドで暗号化され、リレーサーバーが中身を読むことはできません。ただし n0 によれば、IP アドレス、接続時刻、転送データ量などのメタデータはリレー側から観測可能です。
- **サービス品質に関する注意:** n0 の公開リレーは**開発および趣味用途（development and hobby use only）**とされており、SLA や稼働保証はなくレート制限があります。詳細は iroh ドキュメントの [Public Relays](https://docs.iroh.computer/iroh-services/relays/public) と [DNS address lookup](https://docs.iroh.computer/connecting/dns-address-lookup) を参照してください。
- **ログ出力:** `iroh` の内部ログは無効化されています。エージェントのログには接続・拒否・切断などのイベントと、ピアがこの PC で開いたセッションの開始と終了のみが記録され、セッションの中身は記録されません。リモートが決められる文字列はエスケープして短く切り詰め、ログの行数にはレート制限があります（[設定とファイルの保存場所](#configuration-and-files)を参照）。

---

<a id="supported-platforms"></a>
## 対応プラットフォーム

| プラットフォーム | 状態 |
|---|---|
| **macOS (Apple Silicon)** | 実機検証済み。ビルド済みバイナリあり、`launchd` による自動起動に対応。 |
| **macOS (Intel)** | 未検証。ビルド済みバイナリなし（[ソースコードからビルド](#from-source)）。 |
| **Windows 11 (x64)** | 実機検証済み。ビルド済みバイナリあり、`Run` レジストリキーによる自動起動に対応。 |
| **Windows 10 (x64)** | 未検証。 |
| **Windows on ARM** | 未検証。x64 ビルドのみ提供（Herdr の Windows インストーラも x64 専用）。 |
| **Linux** | 未検証。自動起動には非対応（[ソースコードからビルド](#from-source)）。 |

*Windows 接続時の補足:* マシン B が Windows の場合、`hc` は最初に Unix 用の `sh` 接続コマンドを送ります。`herdvia` が `sh` 不在時の `cmd` と同様に終了コード `1` を返すと、`hc` は続いて Windows 用の接続コマンドを送り、`herdvia` がシェルを介さず直接起動します。

---

<a id="configuration-and-files"></a>
## 設定とファイルの保存場所

| 項目 | パス |
|---|---|
| **データフォルダ（macOS）** | `~/Library/Application Support/herdvia` |
| **データフォルダ（Windows）** | `%APPDATA%\herdvia` |
| **データフォルダ（Linux）** | `$XDG_DATA_HOME/herdvia`（既定値 `~/.local/share/herdvia`） |
| **保存先の上書き環境変数** | `HERDVIA_DIR=<パス>` でデータフォルダを変更可能（絶対パスであること。`hc` が起動する `via` を含む全コマンドに同じ値を渡す必要あり。`agent install` とは併用不可） |
| **`secret.key`** | このマシンの 32 バイト Ed25519 秘密鍵（データフォルダ内） |
| **`peers`** | ピア許可リスト（1 行に `<名前> <EndpointId> <in\|out\|both>`、`allow`・`accept`・`revoke` で管理） |
| **`agent.sock`** | macOS/Unix のローカル Unix ドメインソケット（Windows は代わりにユーザー SID 別の名前付きパイプを使用） |
| **`agent.lock`** | Windows のみ。エージェントが実行中にロックするファイルで、1 つのデータフォルダでエージェントが 1 つだけ動くようにします。エージェント停止後も残ります。 |
| **ログ（macOS 自動起動時）** | `~/Library/Logs/herdvia.log` |
| **ログ（Windows 自動起動時）** | `%LOCALAPPDATA%\herdvia\herdvia.log` |
| **ログ（`herdvia agent` 手動起動時）** | 標準エラー出力（`stderr`） |

*注：エージェントが実行中にログに書き込む各行は、`2026-11-14T14:17:03Z herdvia: connected <id>` のように UTC 時刻で始まり、常に 1 行です。改行や ESC などの制御文字はエスケープされ（`\n`、`\u{1b}`）、300 文字を超える文字列は切り詰められて `... [truncated, <N> bytes]` で終わります。未認証の接続に関する行（拒否したピア、失敗した着信ハンドシェイク）は一度に 30 行、その後は 10 秒に 1 行まで、それ以外の行は一度に 60 行、その後は 1 秒に 1 行まで書き込みます。捨てた行があれば、その種類の次の行の前に `suppressed 71 log lines about unauthenticated connections` や `suppressed 3 other log lines` のように捨てた行数を書き込みます。受信側では、ピアが開いたセッションごとに `session started: <名前> (fingerprint <フィンガープリント>), herdr session <セッション>` の行（`hc` がセッションを指定しない場合は `herdr session` の部分なし）と `session ended: <同じ説明>: <理由>` の行を残します。理由は例えば `exit code 0`、`closed`（この PC で `revoke` や権限変更をした場合）、`the agent stopped` です。*

*自動起動されたエージェントは、起動直後とその後 1 時間ごとにログファイルのサイズを確認し、10 MiB を超えていれば末尾 5 MiB に含まれる完全な行だけを残します。また、起動直後とその後 24 時間ごとに 180 日を過ぎた行を削除します。ファイルを分けてローテーションするのではなく、同じファイルの先頭部分だけを削除します。時刻のない行（以前のバージョンが書き込んだ行や、エージェントが起動に失敗したときのエラー行など）は、その次の時刻付きの行と一緒に削除されます。手動起動した `herdvia agent` の `stderr` がターミナルや別のファイルに出力される場合は手を加えません。macOS では手動起動でも `stderr` を `~/Library/Logs/herdvia.log` に追記していれば、同じように削除します。*

---

<a id="design"></a>
## 設計ドキュメント

アーキテクチャメモ、ワイヤーフレーミングの詳細、および実測した `hc` の挙動は [DESIGN.md](../DESIGN.md)（韓国語）にまとめられています。

---

<a id="license"></a>
## ライセンス

あなたの選択により、以下のいずれかのライセンスの下で利用できます：

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) または <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) または <http://opensource.org/licenses/MIT>)

### コントリビューション

明示的な別段の定めがない限り、Apache-2.0 ライセンスの定義に従い本作品に含めるためにあなたが意図的に提出したすべての貢献は、追加の条件なしに上記の通りデュアルライセンスされるものとします。
