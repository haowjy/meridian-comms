# Running the daemon as a service

The daemon must run outside any agent session: started from inside one, it would inherit
that session's `MERIDIAN_*` variables (it strips them before calling Meridian, but a
service is the clean way). Run exactly one per bot; a second refuses to start
([design.md](design.md#slack)).

`meridian-comms` and `meridian` must both be on the service's `PATH`. With `cargo
install --path .` the binary is in `~/.cargo/bin`.

## Linux (systemd user service)

`~/.config/systemd/user/meridian-comms.service`:

```ini
[Unit]
Description=meridian-comms: Slack to agent sessions
After=network-online.target

[Service]
ExecStart=%h/.cargo/bin/meridian-comms daemon
Environment=PATH=%h/.cargo/bin:%h/.local/bin:/usr/local/bin:/usr/bin:/bin
Environment=RUST_LOG=info
Restart=on-failure
# After a crash Slack may still count the dead connection; wait it out.
RestartSec=60

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now meridian-comms
journalctl --user -u meridian-comms -f      # the log
```

To keep it running while you're logged out: `loginctl enable-linger "$USER"`.

## macOS (launchd)

`~/Library/LaunchAgents/dev.meridian-comms.plist`, with `$HOME` written out:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.meridian-comms</string>
  <key>ProgramArguments</key><array>
    <string>/Users/YOU/.cargo/bin/meridian-comms</string><string>daemon</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/Users/YOU/.cargo/bin:/Users/YOU/.local/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
    <key>RUST_LOG</key><string>info</string>
  </dict>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>60</integer>
  <key>StandardErrorPath</key><string>/Users/YOU/Library/Logs/meridian-comms.log</string>
</dict></plist>
```

```sh
launchctl load ~/Library/LaunchAgents/dev.meridian-comms.plist
```
