---
title: 安装 Koharu
description: 安装发行版，完成首次启动并保持更新。
---

# 安装 Koharu

除非你准备修改 Koharu 本身，否则请使用发行版。此仓库只发布 64 位 Windows 安装程序。

## 下载发行版

打开[最新 GitHub Release](https://github.com/proxlavee/koharu-fork/releases/latest)并运行 NSIS 安装程序。安装程序未进行代码签名，Microsoft Defender SmartScreen 可能会显示警告。此仓库不发布 macOS 或 Linux 应用程序。

发布包包含固定版本的 Chromium Embedded Framework 运行时及其资源。Koharu 不需要系统浏览器。

## 首次启动

原生运行时准备完毕后，Koharu 会显示项目浏览器。首次启动可能需要下载原生运行时包，因此耗时更长。具体模型文件会在第一次使用该模型时解析。

下载需要访问 GitHub Release 资源；模型权重通常还需要访问 Hugging Face。进度显示在活动中心。软件包发布到本地缓存期间不要关闭应用。

## 更新

此未签名版本不启用 Tauri 的签名更新器。更新时，请关闭 Koharu，然后从此仓库的 GitHub Releases 页面运行较新的安装程序。

下一步请[翻译第一个项目](/zh-CN/getting-started/first-project/)。硬件选择与缓存行为见[运行时、模型与硬件](/zh-CN/getting-started/runtime-models-and-hardware/)。

如需从源码构建，请转到[开发环境](/zh-CN/development/setup/)。
