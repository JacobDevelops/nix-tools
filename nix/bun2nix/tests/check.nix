{ pkgs }:
assert (import ./eval.nix { inherit pkgs; }) == "bun2nix-nix-eval-tests";
pkgs.runCommand "bun2nix-nix-eval-tests" { } "touch $out"
