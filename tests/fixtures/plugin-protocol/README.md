# Plugin protocol fixtures

This directory holds some of Chaat's protocol fixtures: `hello.json`,
`options.json`, `action.json`, `shutdown.json` and `push_channels.json` (the
messages Chaat sends to this plugin), and `push_ready.json`, `push.json`,
`push_down.json` and `option_status.json` (messages this plugin sends; its
tests compare shapes). One JSON message per file, exactly as sent on the wire
(one line).

Each file is a byte-identical copy of the file of the same name in the Chaat
repo (`tests/fixtures/plugin-protocol/`), which is the source. Chaat's
directory holds more files, for messages this plugin never receives or sends;
they are not copied here. This plugin's tests parse these messages and check
that the messages it sends match the protocol (as JSON values; key order
doesn't matter). When a fixture changes, update it in Chaat and here together.
