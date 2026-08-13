#!/usr/bin/env bun

import { exec as execCallback } from 'node:child_process'
import { readFile, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { promisify } from 'node:util'

const exec = promisify(execCallback)
const root = path.resolve(__dirname, '..')
const execOpts = { cwd: root, maxBuffer: 10 * 1024 * 1024 }

async function resolveGitHubRepository() {
  const configured = (process.env.GITHUB_REPO ?? process.env.GITHUB_REPOSITORY)?.trim()
  if (configured) {
    return configured
  }

  const remote = (await exec('git remote get-url origin', execOpts)).stdout.trim()
  return remote.match(/github\.com[/:]([^/\s]+\/[^/\s]+?)(?:\.git)?$/)?.[1]
}

async function main() {
  const githubRepository = await resolveGitHubRepository()
  const cliffExecOpts = githubRepository
    ? {
        ...execOpts,
        env: { ...process.env, GITHUB_REPO: githubRepository },
      }
    : execOpts
  let bumpedVersion = process.argv[2]?.trim()

  if (bumpedVersion) {
    console.log(`Using provided version: ${bumpedVersion}`)
  } else {
    console.log('Calculating bumped version with git-cliff...')
    bumpedVersion = (
      await exec('bun git-cliff --offline --unreleased --bumped-version', cliffExecOpts)
    ).stdout.trim()
  }

  if (!bumpedVersion) {
    throw new Error('git-cliff did not return a bumped version')
  }

  console.log(`Bumped version: ${bumpedVersion}`)

  const cargoTomlPath = path.join(root, 'Cargo.toml')
  const cargoToml = await readFile(cargoTomlPath, 'utf8')
  const versionPattern = /(\[workspace\.package\][\s\S]*?version\s*=\s*")([^"]+)(")/

  if (!versionPattern.test(cargoToml)) {
    throw new Error('Could not find [workspace.package] version in Cargo.toml')
  }

  const updatedCargoToml = cargoToml.replace(versionPattern, `$1${bumpedVersion}$3`)
  await writeFile(cargoTomlPath, updatedCargoToml)
  console.log('Updated Cargo.toml version')

  await exec('cargo metadata --format-version 1', execOpts)
  console.log('Updated Cargo.lock')

  await exec('git add Cargo.toml Cargo.lock', execOpts)
  await exec(`git commit -m "chore(release): ${bumpedVersion}"`, execOpts)
  console.log('Created release commit')

  await exec(`git tag ${bumpedVersion}`, execOpts)
  console.log('Created git tag')

  await exec(`bun git-cliff --offline -o CHANGELOG.md`, cliffExecOpts)
  console.log('Updated CHANGELOG.md')

  await exec('git add CHANGELOG.md', execOpts)
  await exec(`git commit --amend --no-edit`, execOpts)
  console.log('Amended release commit with updated CHANGELOG.md')

  await exec(`git tag -f ${bumpedVersion}`, execOpts)
  console.log('Updated git tag to include CHANGELOG.md')

  console.log(`Release commit and tag ${bumpedVersion} created.`)
}

main().catch((error) => {
  console.error(error)
  process.exit(1)
})
