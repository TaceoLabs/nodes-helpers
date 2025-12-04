[private]
default:
    @just --justfile {{ justfile() }} --list --list-heading $'Project commands:\n'

lint:
    cargo fmt --all -- --check
    # We do these standalone checks to not have wrong passes due to workspace dependencies
    # So we cd into the subcrate and run the checks as if it was standalone
    just lint-subcrate nodes-common
    just lint-subcrate nodes-observability

lint-subcrate SUBCRATE:
    cd {{ SUBCRATE }} && cargo all-features clippy --all-targets -q -- -D warnings
    cd {{ SUBCRATE }} && RUSTDOCFLAGS='-D warnings' cargo all-features doc -q --no-deps

test:
    cargo test --all-features --all-targets

check-pr: lint test
