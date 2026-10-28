#!/usr/bin/env bash
set -euo pipefail
# Requires: [ nix jq ]
# Regenerate `crates/builtin/snapshot.json`, the fallback used by
# `crates/builtin/build.rs` when `nix` is unavailable at build time (eg. Windows).
# It records the raw outputs of the same Nix queries the build script runs.

if git_dir="$(git rev-parse --show-toplevel)"; then
	cd "$git_dir"
fi

nix_eval=(nix eval --experimental-features 'nix-command flakes' --store dummy:// --impure)

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

nix __dump-language >"$tmp/language.json"
"${nix_eval[@]}" --raw --expr 'builtins.concatStringsSep "\n" (builtins.attrNames builtins)' >"$tmp/names"

# Probe each name in parallel to find global names, same as the build script.
xargs -d '\n' -P 0 -I '{}' sh -c '"$@" --expr "$0" </dev/null >/dev/null 2>&1 && echo "$0" || true' '{}' "${nix_eval[@]}" \
	<"$tmp/names" >"$tmp/globals"

jq -n \
	--arg nixVersion "$(nix --version)" \
	--rawfile names "$tmp/names" \
	--rawfile globals "$tmp/globals" \
	--slurpfile language "$tmp/language.json" \
	'{
    nixVersion: $nixVersion,
    attrNames: ($names | split("\n") | map(select(. != ""))),
    globalNames: ($globals | split("\n") | map(select(. != "")) | sort),
    language: $language[0]
  }' >crates/builtin/snapshot.json
