# Bun monorepo

This example has two runnable apps and one local shared package. `kleur` is shared by every workspace, while `hono` and `preact` are exclusive to one app each, producing three exact-consumer cache shards.

Each production package receives a source cone without its tests. Its check receives the matching test files, and both install from the same lockfile-only offline cache joins.

```sh
bun install
bun test
nix flake check
nix run .#example-api
```
