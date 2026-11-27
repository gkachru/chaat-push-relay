# chaat-push-relay

A plugin for [Chaat](https://github.com/gkachru/chaat) (a small native client that works
with Microsoft Teams). It gives Chaat push updates, so new messages show up
without waiting for you to look, without anyone hosting `chaat-relay`.

Microsoft Graph sends change notifications to a public HTTPS address. This
plugin runs a small receiver on your computer (127.0.0.1 only) and puts it on
the internet through a `cloudflared` tunnel. Graph posts to the tunnel, the
plugin passes each body to Chaat as a protocol line, and Chaat checks it and
fetches what changed, as it does for any push. Chaat's side is described in
[plugins.md](https://github.com/gkachru/chaat/blob/main/docs/plugins.md#push-plugins)
and [realtime.md](https://github.com/gkachru/chaat/blob/main/docs/realtime.md#through-a-push-plugin).

## Status

Early preview. It has been used with Chaat on Windows, receiving real Graph
notifications through a cloudflared tunnel. Its automated tests use a
stand-in for cloudflared and local requests. CI builds and tests it on Linux
and Windows.

## Install

```
cargo build --release --locked
```

The program is `target/release/chaat-push-relay` (on Windows,
`target\release\chaat-push-relay.exe`). Chaat never downloads or updates a
plugin; you build or install it yourself.

You also need `cloudflared`. Install it yourself from Cloudflare. The plugin
never installs or updates it. It uses the file named in the "cloudflared
program" option, or else looks for `cloudflared` (or `cloudflared.exe`) on
your PATH.

## Install on Debian or Ubuntu

For Ubuntu 22.04 or later and Debian 12 or later (amd64; the package needs
glibc 2.35 or later). Build the package with `packaging/deb.sh` (needs
`cargo-deb`, `cargo-zigbuild` and `zig`), then:

```
sudo apt install ./dist/chaat-push-relay_0.1.0-1_amd64.deb
```

The package installs the program as `/usr/lib/chaat/plugins/chaat-push-relay`
and this README in `/usr/share/doc/chaat-push-relay/`. It depends on `libc6`
and `ca-certificates`, recommends `cloudflared` and suggests `chaat`.

In Chaat, open Settings › Plugins, choose **Add plugin…** and pick
`/usr/lib/chaat/plugins/chaat-push-relay`. After you upgrade the package,
Chaat sees a changed file and does not run it until you choose **Run the new
file**. That clears the plugin's approvals, so you allow its `push` need
again.

`cloudflared` is not in the Debian or Ubuntu archives. The package only
recommends it. Add Cloudflare's APT repository by following the instructions at
https://pkg.cloudflare.com/ (check the current steps there), then
`sudo apt install cloudflared`. With that repository set up, apt also installs
it as a recommended package. The plugin itself never installs or updates
cloudflared; apt does, when you ask it to.

## Add it in Chaat

In Chaat, open Settings › Plugins, choose **Add plugin…** and pick the
`chaat-push-relay` program. Chaat starts it as `chaat-push-relay serve`; the
plugin also runs with no arguments, and anything else prints a usage line
and exits. The plugin calls itself "Push relay". Under **It asks for**, tick
its one need, which Chaat words as "your update notifications: which chats
changed and when, no message text", then choose **Done**.

Once the tunnel is up, the plugin's row in Settings › Plugins and Settings ›
Push updates show "Tunnel at <host>". If you have entered a relay address in
Settings › Push updates, that address wins and the plugin is not used. If
several plugins offer push, only the first running one in the list is used.

## Tunnels

The plugin runs cloudflared with these arguments, where `<port>` is the
receiver's port on 127.0.0.1 (a free port picked each time the plugin
starts):

```
quick tunnel:              tunnel --no-autoupdate --protocol http2 --url http://127.0.0.1:<port>
named tunnel:              tunnel --no-autoupdate --protocol http2 run --url http://127.0.0.1:<port> <name or ID>
named tunnel, config file: tunnel --no-autoupdate --config <file> --protocol http2 run --url http://127.0.0.1:<port>
```

For both kinds the tunnel counts as up only after cloudflared reports a
registered connection, so Graph's first validation request can reach it.

When Chaat sends `shutdown` or closes the plugin's input, the plugin stops
cloudflared and exits. Chaat also contains the plugin: on Windows it runs
each plugin in a Job Object that kills its processes when Chaat ends the
plugin's run, and on Unix in its own process group, which Chaat kills as a
group. So cloudflared does not outlive a plugin that Chaat stops or kills.

**Quick tunnel (default).** No account needed. cloudflared gives a
`trycloudflare.com` address that changes each time the tunnel starts. Chaat
subscribes again with the new address each time.

**Named tunnel.** The address stays the same. Set it up once with
cloudflared:

```
cloudflared tunnel login
cloudflared tunnel create <name>
cloudflared tunnel route dns <name> <hostname>
```

Then set Tunnel to Named tunnel, Tunnel name or config to the tunnel name (or
the path of its config file, ending in `.yml`, `.yaml` or `.json`), and
Public hostname to the hostname. The plugin reports `https://<hostname>` to
Chaat as the tunnel's address.

Starting a tunnel by name makes cloudflared look the name up through
Cloudflare's API, so the tunnel cannot start while that API fails. The
plugin remembers the tunnel ID cloudflared prints the first time the tunnel
comes up by name, and later starts use the ID, which needs only the local
credentials file. If a start by the remembered ID fails before the tunnel is
up, the plugin forgets it and the next start goes by name. You can also
enter the tunnel's ID instead of its name. Nothing is remembered for a config
file or an ID.

## Options

| Option | Meaning |
|---|---|
| Tunnel | Quick tunnel (default) or Named tunnel |
| cloudflared program (advanced) | The full path of cloudflared, if it is not on your PATH |
| Tunnel name or config | Named tunnel only. A tunnel name or ID, or the path of its config file |
| Public hostname | Named tunnel only. The hostname the tunnel is published at |
| Check the tunnel | Button. Sends a test request through the public address |

Changing a tunnel option restarts the tunnel. If it was up, Chaat is told it
went down with the reason "tunnel options changed". If Chaat sends no options
within 3 seconds of the plugin's `ready`, the plugin starts a quick tunnel.

Status texts shown under Tunnel:

| Text | When |
|---|---|
| Starting the tunnel | cloudflared is starting |
| Tunnel up at `<host>` | The tunnel is up |
| cloudflared was not found | No file at the option's path, or no cloudflared on PATH. No retry until you change the options |
| Set the tunnel's public hostname | Named tunnel without a hostname |
| Set the tunnel's name | Named tunnel with a hostname but without a name or config |
| cloudflared stopped | cloudflared exited. When it printed an error line, the last one follows ("cloudflared stopped: ...", the error cut to 160 characters). It restarts after 1 second, then 2, 4 and so on up to 60 seconds. A tunnel that ran for a minute starts over at 1 second |
| cloudflared could not start | The program could not be run. Same restart schedule |

Each time the tunnel stops, Chaat is also told it went down, with the same
text as the reason.

Status texts shown under Check the tunnel:

| Text | When |
|---|---|
| Checking the tunnel | The test request is in flight (10 second limit) |
| The tunnel answered | The public address reached this plugin |
| The tunnel did not answer | No answer, an error, or the wrong answer |
| The tunnel is not up yet | You pressed the button before the tunnel was up, or the tunnel went down after a check passed |

The check posts a validation request with random tokens to
`<address>/graph/<token>`. If a check failed, it runs again when the tunnel
next comes up.

## Privacy

- The plugin sees what Graph posts: subscription IDs, `clientState`, change
  types and resource paths such as `chats('19:...')/messages('...')`. Chaat's
  subscriptions carry no resource data, so there is no message text, and
  the plugin never has your tokens.
- Cloudflare carries the same requests, so it sees the same content, plus
  the request paths (which hold the channel names) and the timing.
- Chaat sends the plugin `hello`, its own option values, one random channel
  name per account (no account IDs), button clicks and `shutdown`. It sends
  no tokens, account IDs, sign-in names or message text. The plugin accepts
  a request only on one of those channels.
- The plugin sends Chaat its name, description and options, statuses, the
  tunnel's address when it is up, the notification bodies, and the reason
  when the tunnel goes down.
- The plugin answers Graph's validation requests itself, by echoing the
  token, and only on announced channels. They are not passed to Chaat.
- Its only network traffic of its own is the receiver on 127.0.0.1 and the
  "Check the tunnel" request to its public address. cloudflared makes its own
  connections to Cloudflare. The plugin has no telemetry.
- The plugin writes at most two fixed lines to stderr (a usage line, and a
  line when it cannot talk to Chaat). It never logs bodies, channels,
  addresses or cloudflared's output. Chaat discards plugin stderr.
- It writes one file: `tunnel-ids.json`, mapping named tunnels to their IDs
  (through a temporary `tunnel-ids.json.tmp` that is renamed into place), in
  `%LOCALAPPDATA%\chaat-push-relay` on Windows,
  `~/Library/Application Support/chaat-push-relay` on macOS, and
  `$XDG_STATE_HOME/chaat-push-relay` (or `~/.local/state/chaat-push-relay`)
  elsewhere. Tunnel IDs are not secret. The plugin never reads cloudflared's
  own credential files; cloudflared does.
- cloudflared's error lines are passed to Chaat as the tunnel's status and
  down reason, cut to 160 characters. They can name hosts and IP addresses.

## Limits

- Only POST to `/graph/<channel>` and `/lifecycle/<channel>` is accepted.
  Anything else gets a 404: other paths and methods, and unknown channels.
- Bodies over 512 KiB are refused (413). An accepted body gets 202.
- A body of up to 48 KiB normally goes to Chaat in one line. A larger body
  in Graph's `{"value":[...]}` form is repacked into several bodies of that
  form, each in its own line; other top-level fields are left out, and an
  entry too large for one line is dropped. A larger body in any other form
  is dropped. Each line stays under 64 KiB.
- Up to 256 bodies wait for Chaat; more are dropped. Graph still gets 202.

## Development

`tests/fixtures/plugin-protocol/` holds byte-identical copies of some of the
fixtures in Chaat's repository, which are the source: when one changes,
change it in Chaat and copy it here. `hello.json` is an example message; its
capabilities and account are sample values, and Chaat sends this plugin an
empty account list since it does not ask for accounts. The protocol is
described in Chaat's
[plugins.md](https://github.com/gkachru/chaat/blob/main/docs/plugins.md).

CI runs:

```
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

## Security

To report a security issue, see `SECURITY.md`.

## License

MIT. See `LICENSE`.
