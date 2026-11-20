# chaat-push-relay

A plugin for [Chaat](https://github.com/gkachru/chaat) (a small native client that works
with Microsoft Teams). It gives Chaat push updates, so new messages show up
without waiting for you to look, without anyone hosting `chaat-relay`.

Microsoft Graph sends change notifications to a public HTTPS address. This
plugin runs a small receiver on your computer (127.0.0.1 only) and puts it on
the internet through a `cloudflared` tunnel. Graph posts to the tunnel, the
plugin passes each body to Chaat as a protocol line, and Chaat checks it and
fetches what changed, as it does for any push.

## Status

Draft. It is tested with a stand-in for cloudflared and with local requests
only. It has not yet been tried against a real tunnel or real Graph
notifications. Expect to adjust things when you try it.

## Install

```
cargo build --release
```

The program is `target/release/chaat-push-relay` (on Windows,
`target\release\chaat-push-relay.exe`). Chaat never downloads or updates a
plugin; you build or install it yourself.

You also need `cloudflared`. Install it yourself from Cloudflare. The plugin
never installs or updates it. It looks for `cloudflared` on your PATH, or at
the path in the "cloudflared program" option.

## Add it in Chaat

In Chaat, open Settings > Plugins, choose Add plugin and pick the
`chaat-push-relay` program, then turn it on. Chaat asks you to approve what
the plugin needs: your update notifications (`push`). Once the tunnel is up,
Settings > Push updates shows it. If you have entered a relay address in
Chaat, that address wins and the plugin is not used.

## Tunnels

The plugin always runs cloudflared with `--protocol http2`.

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

Then set the options below to the tunnel name (or the path of its config
file, ending in `.yml`, `.yaml` or `.json`) and the public hostname.

## Options

| Option | Meaning |
|---|---|
| Tunnel | Quick tunnel (default) or Named tunnel |
| cloudflared program (advanced) | The full path of cloudflared, if it is not on your PATH |
| Tunnel name or config | Named tunnel only. A tunnel name, or the path of its config file |
| Public hostname | Named tunnel only. The hostname the tunnel is published at |
| Check the tunnel | Button. Sends a test request through the public address |

Changing a tunnel option restarts the tunnel. If it was up, Chaat is told it
went down with the reason "tunnel options changed".

Status texts shown under Tunnel:

| Text | When |
|---|---|
| Starting the tunnel | cloudflared is starting |
| Tunnel up at `<host>` | The tunnel is up |
| cloudflared was not found | No cloudflared at the option's path or on PATH. No retry until you change the options |
| Set the tunnel's public hostname | Named tunnel without a hostname |
| Set the tunnel's name | Named tunnel without a name or config |
| cloudflared stopped | cloudflared exited. It restarts after 1 second, then 2, 4 and so on up to 60 seconds. A tunnel that ran for a minute starts over at 1 second |
| cloudflared could not start | The program could not be run. Same restart schedule |

Status texts shown under Check the tunnel:

| Text | When |
|---|---|
| Checking the tunnel | The test request is in flight (10 second limit) |
| The tunnel answered | The public address reached this plugin |
| The tunnel did not answer | No answer, an error, or the wrong answer |
| The tunnel is not up yet | You pressed the button before the tunnel was up |

## Privacy

- The plugin sees what Graph posts: subscription IDs, `clientState`, change
  types and resource paths such as `chats('19:...')/messages('...')`. Chaat's
  subscriptions carry no resource data, so there is no message text, and
  the plugin never has your tokens.
- Cloudflare carries the same requests, so it sees the same content, plus
  the timing.
- The plugin never logs bodies, channels or addresses with a query. Logs go
  to stderr, which Chaat discards. It has no telemetry.
- Chaat sends the plugin only its own channel names for each account, and the
  plugin accepts a request only on one of those channels.

## Limits

- Bodies over 512 KiB are refused (413).
- A body is split into several lines so that each carries at most 48 KiB of
  body, and each whole line stays under 64 KiB.
- Anything else gets a 404: other paths and methods, and unknown channels.
- Up to 256 bodies wait for Chaat; more are dropped.

## Development

`tests/fixtures/plugin-protocol/` holds copies of the fixtures in Chaat's
repository. Keep them identical to Chaat's. The protocol is described in
Chaat's `docs/plugins.md`.

```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT. See `LICENSE`.
