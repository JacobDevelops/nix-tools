# Bun monorepo

This example has two runnable apps and one local shared package. `kleur` is shared by every workspace, while `hono` and `preact` are exclusive to one app each, producing three exact-consumer cache shards.

Each production package receives a source cone without its tests and a production-only cache. Checks receive the matching test files and dev dependencies; `preact` is deliberately dev-only for the API so the frozen offline builds prove it is absent from that production cache.

```sh
bun install
bun test
nix flake check
nix run .#example-api
```
