# HW-VERIFICATION — what needs the machine, and how to do it in one command

Everything here was blocked on the box being off. It is scripted so that
turning it on is the only manual step:

```sh
sudo ./scripts/hw-verification.sh deploy /tmp/zinit-artifact --sha256 <hash>
sudo ./scripts/hw-verification.sh soak 24
sudo ./scripts/hw-verification.sh soak-report
sudo ./scripts/hw-verification.sh power-test      # powers the machine off
sudo ./scripts/hw-verification.sh verify-power    # after the next boot
```

## What each part proves, and what it cannot

| Step | Proves | Cannot prove |
|---|---|---|
| `deploy` | the artifact is intact (sha256), installed by rename, and the running supervisor is the new one | that the new image survives a boot as PID 1 — `deploy` refuses to guess |
| `verify` | control socket answers, no failed services, no zombies, RSS and fd counts | that services behave under load |
| `soak` | 24 h of samples with restarts and reloads: no failed service, no zombie, RSS growth < 2 MB | that a service's own workload is correct |
| `power-test` | that `SIGTERM` to PID 1 takes the machine down in an orderly way, with a breadcrumb on disk written *before* the signal | that the machine comes back — that is the boot after, and `verify-power` reads it |
| `verify-power` | the breadcrumb exists and the supervisor answers after the reboot | that the shutdown order was correct in every service |

Two things the script deliberately refuses to fake:

* **No hash, no install.** A truncated download that still executes is the
  one failure mode that looks like success.
* **No `--yes` for the power test.** It types the word. `SIGTERM` to PID 1 is
  a poweroff by contract; the marker is written and `sync`ed before the
  signal, because nothing after it is guaranteed to run.

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
