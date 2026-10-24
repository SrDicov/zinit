# BETA — what has to be true before the tag, and how the tag happens

Alpha is published (`v0.2.0-alpha`, `32c92d1`). Beta is **not** gated on code
in this repo: it is gated on evidence from a machine that boots with zinit as
PID 1. Anything here that can be done without the box is already done and
green; see `docs/CI.md` and `docs/audit/mutants-costs.md`.

## The five pre-requisites

Each one has a command and an artifact. "It worked when I tried" is not
evidence; the file is.

| # | Pre-requisite | Command | Evidence |
|---|---|---|---|
| 1 | `ci.yml` green on the commit being tagged (only `riscv32gc` may be red: upstream `std` does not exist for that target) | `gh run list --branch main --workflow ci` | the run URL in the release notes |
| 2 | `distro.yml` green on that same commit, or each red leg explained in `docs/CI.md` as an environment limit | `gh run list --branch main --workflow distro` | run URL + a line per red leg |
| 3 | 24 h soak on the box: zero failed services, zero zombies, RSS growth < 2 MB | `sudo ./scripts/hw-verification.sh soak 24` then `soak-report` | the PASS block from `soak-report`, plus `/var/log/zinit-soak.tsv` |
| 4 | Both power paths, each followed by a successful boot: `zctl poweroff` and `SIGTERM` to PID 1 | `full` (ends in `kill-term-1`) + `after-reboot` | the breadcrumb in `/var/log/zinit-power.log` and `verify-power` PASS |
| 5 | zinit is the only init: no dinit binary, no `/etc/dinit.d`, no `devmon` still starting | `sudo ./scripts/hw-verification.sh dinit-removal-check` | its output, and the package list after removal |

Note on 5: `devmon` ships in `dinit-base` and is what automounts removable
media (`KNOWN_ISSUES.md`). Keeping the package while zinit is PID 1 is an
accepted configuration, but then the entry in `KNOWN_ISSUES.md` is part of
the release notes, not a silent detail.

## The tag

A tag without a published GitHub release is not a release; the workflow does
both, so the order is fixed:

```sh
# 1. version, on a branch, through the normal gate
#    Cargo.toml [workspace.package] version = "0.2.0-beta"
cargo build --workspace --all-targets
cargo clippy --all-targets            # 0 warnings
cargo fmt --all --check
cargo test --workspace
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(release): 0.2.0-beta"
# push the branch, open the PR, merge when CI is green

# 2. annotated tag on the merged main commit
git checkout main && git pull --ff-only
git tag -a v0.2.0-beta -m "v0.2.0-beta: first beta, hardware-verified"
git push origin v0.2.0-beta           # this triggers .github/workflows/release.yml

# 3. the human notes, after the workflow publishes the artifacts
gh release edit v0.2.0-beta --notes-file RELEASE_NOTES.md
```

Rules that are not negotiable:

- **Never rewrite a published tag.** `v0.2.0-alpha` carries a flaky test
  (`KNOWN_ISSUES.md`); it stays as published. The fix is in `main`.
- **Never tag with a red gate.** `ci.yml` red is a stop; `distro.yml` red is a
  documented finding, one line per leg, in the release notes.
- **Never tag from a branch.** The tag lands on the merge commit of `main`,
  which is the tree CI tested.
- **No artifacts by hand.** The workflow builds the five static musl targets,
  packs `scripts/install.sh` with each one, and writes the `.sha256` files. A
  binary built on a laptop is not a release.
- **Beta is not "0.2.0 final".** If a beta fails hardware, the fix ships as
  `0.2.0-beta.2`; the tag that failed stays published and the release notes
  say why.

## What beta promises

- A static `zinit` that can be PID 1 on x86_64/i686/aarch64 (+ the two extra
  targets the release builds), verified by 24 h of soak on real hardware.
- Both shutdown paths verified across a reboot, not just "the call returned".
- The dinit migration path stated plainly, including the `devmon` exception.

It does **not** promise: mount namespaces (deliberately deferred), the
remaining mutants (`docs/audit/mutants-costs.md`), or any service-manager
behaviour that was never observed on hardware.
