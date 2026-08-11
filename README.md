<a href="https://signal-cli.vercel.app"><img src="./website/screenshot.png" alt="current" width="100%"/></a>

# signal-cli

a fast, keyboard-first signal client for your terminal.

```sh
curl https://signal-cli.vercel.app/install.sh -sSf | sh
```

The installer keeps the managed binary in `~/.local/lib/signal-cli` and links
it into `/usr/local/bin`. Stable releases are downloaded automatically and take
effect the next time `signal` starts. Set `SIGNAL_NO_UPDATE=1` to disable update
checks.
