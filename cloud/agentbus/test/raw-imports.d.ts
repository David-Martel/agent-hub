// Vite's `?raw` suffix import (build-time file-to-string transform, used by
// security.test.ts to assert on wrangler.toml content without a runtime
// filesystem read, which the Workers runtime doesn't have).
//
// Kept in its OWN file, separate from env.d.ts's `declare global` block: a
// `declare module "*?raw"` ambient wildcard declaration placed in a file that
// ALSO contains `declare global` + `export {}` is silently ignored by
// TypeScript's bundler module resolution for every OTHER file in the
// program, even though the declaration itself is well-formed (confirmed via
// a minimal repro) — apparently an interaction between global augmentation
// scoping and ambient wildcard module declarations in the same file/module.
declare module "*?raw" {
  const content: string;
  export default content;
}
