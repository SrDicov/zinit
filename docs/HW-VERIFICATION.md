# HW-VERIFICATION — what needs the machine, and how to do it in one command

Everything here was blocked on the box being off. It is scripted so that
turning it on is the only manual step.

## The night, two commands

```sh
sudo ./scripts/hw-verification.sh full /tmp/zinit-artifact --sha256 <hash>
# the machine powers itself off at the end; bring it back to the zinit entry
sudo ./scripts/hw-verification.sh after-reboot
```

`full` chains `deploy` → `soak 24` → `soak-report` → `kill-term-1` (the
shutdown test that ends the boot). The second half can only run after the
machine is back, so it is a separate command. Neither is reversible.

Every step still runs on its own when something fails:

```sh
sudo ./scripts/hw-verification.sh deploy /tmp/zinit-artifact --sha256 <hash> [--dry-run]
sudo ./scripts/hw-verification.sh verify
sudo ./scripts/hw-verification.sh soak 24
sudo ./scripts/hw-verification.sh soak-report
sudo ./scripts/hw-verification.sh power-test      # powers off via zctl (operator verb)
sudo ./scripts/hw-verification.sh kill-term-1     # powers off via SIGTERM to PID 1
sudo ./scripts/hw-verification.sh verify-power    # after the next boot
sudo ./scripts/hw-verification.sh dinit-removal-check
sudo ./scripts/hw-verification.sh after-reboot    # verify-power + dinit-removal-check
```

`--dry-run` applies to `deploy` and `full` and touches nothing.

## What each part proves, and what it cannot

| Step | Proves | Cannot prove |
|---|---|---|
| `deploy` | the artifact is intact (sha256), installed by rename, and the running supervisor is the new one | that the new image survives a boot as PID 1 — `deploy` refuses to guess |
| `verify` | control socket answers, no failed services, no zombies, RSS and fd counts | that services behave under load |
| `soak` | 24 h of samples with restarts and reloads: no failed service, no zombie, RSS growth < 2 MB | that a service's own workload is correct |
| `power-test` | that the operator verb (`zctl poweroff`) takes the machine down in an orderly way, with a breadcrumb written *before* it | that the kernel-owned signal path works — that is `kill-term-1` |
| `kill-term-1` | that `SIGTERM` to PID 1 (the kernel's power signal) powers off in an orderly way | that Ctrl-Alt-Del reboots (`SIGINT` is the other half, same handler) |
| `verify-power` | the breadcrumb exists and the supervisor answers after the reboot | that the shutdown order was correct in every service |
| `dinit-removal-check` | read-only: which dinit binary, config, process or `devmon` still own the machine | that removing them is safe — it only names what would break |
| `after-reboot` | `verify-power` + `dinit-removal-check` in one pass | — |

Two things the script deliberately refuses to fake:

* **No hash, no install.** A truncated download that still executes is the
  one failure mode that looks like success.
* **No `--yes` for a power test.** It types the word `poweroff`. The marker is
  written and `sync`ed before the trigger, because nothing after it is
  guaranteed to run.

## Deploying as PID 1 vs as a supervised process

`zinit --init` is PID 1 and cannot be restarted in place; the script says so
and stops after installing, leaving the reboot to the operator (`grub-reboot`
to the zinit entry, as in `STATUS.md`). `zinit --sup` can be restarted, and
the script checks that its launcher brings it back in under five seconds —
if nothing does, that is a finding about the launcher, not about zinit.

## Numbers to record in the verdict

`soak-report` prints samples, RSS first→last, fd first→last, and the count of
samples with a failed service or a zombie. PASS means zero of those and less
than 2 MB of RSS growth. Anything else is a verdict in the other direction,
with the TSV to say where.

## Do not run the power tests on a development machine

This desktop (Artix) runs dinit as PID 1. `dinit-removal-check` is safe and
answers "no"; `power-test`, `kill-term-1`, `soak` and `full` belong on the
box that will actually run zinit as PID 1. `full` checks `/proc/1/comm` before
the shutdown test and refuses elsewhere.
