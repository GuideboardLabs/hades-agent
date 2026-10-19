# Installing Hades from this tarball

Hades is one binary and one home. This tarball contains the binary, the installer and this file.

```sh
tar xzf hades-<version>-<triple>.tar.gz
cd hades-<version>-<triple>
./install.sh
```

What that writes:

- `~/.local/bin/hades` — the binary (`--prefix` changes it)
- `~/.hades/` — config.json, `mneme/` (the vault), sessions, approvals (`--home` changes it)
- `~/hades-work/` — the empty working tree a run is rooted in (`--root` changes it)
- `~/.config/systemd/user/hades-daemon.service` and `hades-gateway.service`

Then, when a systemd user manager answers, it runs `systemctl --user daemon-reload` and
`systemctl --user enable --now hades-daemon.service`.

The Telegram gateway is **not** started by default. It needs `HADES_TELEGRAM_TOKEN` in `~/.hades/.env`
and a `gateway.telegram` section in `~/.hades/config.json` with non-empty `allowed_users` and `verify`.
Once those exist:

```sh
systemctl --user enable --now hades-gateway.service
journalctl --user -u hades-gateway -f
```

A second run of `install.sh` replaces the binary and the units and leaves an existing home alone. It never
runs `sudo`, never downloads anything, and never reads `~/.hermes`.

Keys live in `~/.hades/.env`, never in `config.json`. See `hades models` for the presets this build ships.

## Stopping, and removing it

```sh
systemctl --user disable --now hades-gateway.service
systemctl --user disable --now hades-daemon.service
rm -f ~/.local/bin/hades ~/.config/systemd/user/hades-daemon.service ~/.config/systemd/user/hades-gateway.service
systemctl --user daemon-reload
```

`~/.hades` and `~/hades-work` are yours to keep or delete; nothing else is left.
