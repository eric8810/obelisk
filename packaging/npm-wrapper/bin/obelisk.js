#!/usr/bin/env node
// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Platform-binary dispatch: resolve the prebuilt obelisk binary for this
// platform package, spawn it, and propagate the exit code. If the platform
// package is missing (unsupported platform), fail honestly.

import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

const platform =
  process.platform === 'linux' ? 'linux' :
  process.platform === 'darwin' ? 'macos' :
  process.platform === 'win32' ? 'windows' : null;
const arch = process.arch === 'x64' ? 'x86_64' :
  process.arch === 'arm64' ? 'aarch64' : null;

if (!platform || !arch) {
  process.stderr.write(`Obelisk has no prebuilt binary for ${process.platform}-${process.arch}.\n`);
  process.exit(1);
}

const binary = join(
  here,
  '..',
  'node_modules',
  `@obelisk-apps/cli-${platform}-${arch}`,
  platform === 'windows' ? 'obelisk.exe' : 'obelisk',
);

if (!existsSync(binary)) {
  process.stderr.write(`The Obelisk platform package is missing (${platform}-${arch}).\n`);
  process.exit(1);
}

const child = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });
process.exit(child.status ?? (child.error ? 1 : 0));
