---
title: Koharu のインストール
description: リリース版をインストールし、初回起動と更新を行います。
---

# Koharu のインストール

Koharu 自体を変更する目的でなければ、リリース版を使用してください。この Repository は 64-bit Windows Installer のみを公開します。

## リリースを入手する

[最新の GitHub リリース](https://github.com/proxlavee/koharu-fork/releases/latest)を開き、NSIS Installer を実行します。この Installer はコード署名されていないため、Microsoft Defender SmartScreen が警告を表示する場合があります。この Repository は macOS または Linux の Application Binary を公開しません。

リリースパッケージには固定した Chromium Embedded Framework ランタイムとリソースが含まれます。Koharu はシステムブラウザーを必要としません。

## 初回起動

ネイティブランタイムの準備が完了すると、プロジェクトブラウザーが表示されます。初回はネイティブパッケージをダウンロードするため、通常より時間がかかることがあります。各モデルのファイルは、そのモデルを初めて使用するときに解決されます。

ダウンロードには GitHub のリリースアセットと、モデル重みの場合は Hugging Face への接続が必要です。進捗はアクティビティセンターに表示されます。パッケージをローカルキャッシュへ公開している途中で終了しないでください。

## 更新

この署名なしビルドでは、Tauri の署名付きアップデーターを有効にしていません。更新するには Koharu を終了し、この Repository の GitHub Releases から新しい Installer を実行してください。

次は[最初のプロジェクト](/ja-JP/getting-started/first-project/)を作成します。ハードウェア選択とキャッシュについては[ランタイム、モデル、ハードウェア](/ja-JP/getting-started/runtime-models-and-hardware/)を参照してください。

ソースからビルドする場合は[開発環境のセットアップ](/ja-JP/development/setup/)へ進んでください。
