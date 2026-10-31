// The platforms restate-session-objects ships its addon for, each a package of its
// own (npm/<platform>, `restate-session-objects-<platform>`) that restate-session-objects
// depends on optionally: a package manager installs the one of its machine
// alone (by the `os`, `cpu` and `libc` of each). The CI builds them
// (.github/workflows/packages.yml); `pnpm build` builds this machine's.

/** The platforms built, as their packages name them. */
export const PLATFORMS = ['linux-x64-gnu', 'linux-arm64-gnu', 'darwin-arm64'] as const;

/** This machine's platform, as a package of the addon would name it. */
export function platform(): string {
  if (process.platform !== 'linux') return `${process.platform}-${process.arch}`;
  const report = process.report.getReport() as { header?: { glibcVersionRuntime?: string } };
  return `linux-${process.arch}-${report.header?.glibcVersionRuntime ? 'gnu' : 'musl'}`;
}
