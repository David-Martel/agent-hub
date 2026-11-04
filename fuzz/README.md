# agent-bus fuzz targets

Coverage-guided fuzzing of the `agent-bus-core` code that takes bytes or text it does not control: stream entries read
back from Redis, hub candidate configuration, MCP tool arguments, message fields, output encodings and network-location
rules. Each target also asserts properties (a round trip, an independent restatement of a rule, an invariant), so a wrong
answer fails as well as a crash.

| Target | Input | Properties |
| --- | --- | --- |
| `stream_entry` | Redis field maps of any value shape | no panic; `_compressed=lz4` bodies round trip; a hostile LZ4 size prefix is cheap; an unrecognised `sensitivity` fails closed |
| `candidates` | `server_urls` JSON | accepted URLs are http(s) with no userinfo, query, fragment or whitespace; no duplicate endpoints; private IPv4 and IPv6 are never "public" |
| `codecs` | MessagePack bytes, messages | decode never panics; encode/decode round trips; a TOON record is one line; excerpts and sanitiser keep their contracts |
| `validation` | message fields, schemas, metadata | `non_empty` bounds; metadata is an object; `auto_fit_schema` output passes its own validator; resource names are bounded |
| `tool_args` | MCP tool call arguments | accepted arguments satisfy the tool's input schema as an independent check reads it |
| `network_location` | CIDR text, location rules, `resolv.conf` | CIDR matches interval arithmetic for both IP families; `Display` re-parses; fingerprint is order independent |

The crate is excluded from the workspace (it declares its own) because `cargo-fuzz` needs a nightly toolchain. Use a
Linux host with many cores; the targets also build on Windows but run far slower. No target opens a connection, so none
needs Redis or PostgreSQL, and none touches the live bus.

```bash
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
cd fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run stream_entry -- -max_total_time=300 -fork=8
```

A failure writes its input to `artifacts/<target>/`. Turn it into a unit test in `agent-bus-core` before fixing it.
