// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Flat ESLint config for the Obelisk root. Scope: the DSH plugin (the one
// remaining TS package, see ADR-0013 Stage 3), the packaging scripts, and
// the E2E harness sources.

import js from '@eslint/js';
import globals from 'globals';
import tseslint from 'typescript-eslint';

export default tseslint.config(
  {
    ignores: [
      'node_modules/**',
      'dist/**',
      'release/**',
      '.dev.docs/**',
      '.obelisk/**',
      '.claude/**',
    ],
  },
  js.configs.recommended,
  {
    files: ['**/*.{js,mjs}'],
    languageOptions: {
      ecmaVersion: 2023,
      sourceType: 'module',
      globals: { ...globals.node },
    },
    rules: {
      // Empty catch is an intentional pattern here (best-effort JSON.parse etc.).
      'no-empty': ['error', { allowEmptyCatch: true }],
      'no-unused-vars': ['warn', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
    },
  },
  {
    files: ['**/*.ts'],
    extends: [...tseslint.configs.recommended],
    languageOptions: {
      globals: { ...globals.node },
    },
    rules: {
      '@typescript-eslint/no-unused-vars': ['warn', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
      // Provider adapters parse untyped external transcript JSON; `any` at those
      // boundaries is deliberate, not a smell.
      '@typescript-eslint/no-explicit-any': 'off',
    },
  },
);
