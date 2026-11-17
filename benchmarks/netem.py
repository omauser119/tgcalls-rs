"""Run a command inside a disposable Linux network namespace with real netem loss/jitter.

Requires root and iproute2. Only the new namespace's lo device is changed.
There are no qdisc changes to the host's network interfaces. Nothing executes on import.
"""
import argparse
import json
import os
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--loss", type=float, default=0)
    parser.add_argument("--delay-ms", type=int, default=0)
    parser.add_argument("--jitter-ms", type=int, default=0)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command or not 0 <= args.loss <= 100 or not 0 <= args.jitter_ms <= args.delay_ms <= 2000:
        parser.error("provide a command and valid loss/delay/jitter values")
    if os.geteuid() != 0:
        parser.error("netem requires root; it operates only in a disposable network namespace")
    namespace = f"tgcalls-bench-{os.getpid()}"
    subprocess.run(["ip", "netns", "add", namespace], check=True)
    try:
        subprocess.run(["ip", "-n", namespace, "link", "set", "lo", "up"], check=True)
        qdisc = ["ip", "netns", "exec", namespace, "tc", "qdisc", "add", "dev", "lo", "root", "netem",
                 "delay", f"{args.delay_ms}ms", f"{args.jitter_ms}ms", "loss", f"{args.loss}%"]
        subprocess.run(qdisc, check=True)
        env = dict(os.environ, TGCALLS_NETEM=json.dumps(dict(loss=args.loss, delay_ms=args.delay_ms, jitter_ms=args.jitter_ms)))
        return subprocess.run(["ip", "netns", "exec", namespace, *command], env=env).returncode
    finally:
        subprocess.run(["ip", "netns", "delete", namespace], check=True)


if __name__ == "__main__":
    raise SystemExit(main())
