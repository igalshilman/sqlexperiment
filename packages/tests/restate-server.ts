// The restate-server the tests against a Restate server start: RESTATE_SERVER
// if set, or else the release that @restatedev/restate-server installs for
// this platform (its version is in package.json).
import { createRequire } from 'node:module';

const fromPackage = createRequire(createRequire(import.meta.url).resolve('@restatedev/restate-server/package.json'));

export const RESTATE_SERVER =
  process.env.RESTATE_SERVER ??
  fromPackage.resolve(`@restatedev/restate-server-${process.platform}-${process.arch}/bin/restate-server`);
