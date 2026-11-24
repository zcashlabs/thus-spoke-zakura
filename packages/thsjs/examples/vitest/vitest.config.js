import { defineConfig } from 'vitest/config';

// Starting a sandbox takes longer than Vitest's 5-second default.
export default defineConfig({ test: { testTimeout: 240_000, hookTimeout: 240_000 } });
