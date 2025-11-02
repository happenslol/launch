@default:
	just --justfile {{justfile()}} --list

set shell := ["bash", "-eo", "pipefail", "-c"]

[no-exit-message]
@dev:
  watchexec -r -e rs,toml -- cargo run
