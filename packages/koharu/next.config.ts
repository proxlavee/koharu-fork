import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import type { NextConfig } from 'next'

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../..')

const nextConfig: NextConfig = {
  devIndicators: false,
  transpilePackages: ['@koharu/bridge', '@koharu/ui'],
  turbopack: {
    root: repositoryRoot,
  },
  output: 'export',
  images: {
    unoptimized: true,
  },
}

export default nextConfig
