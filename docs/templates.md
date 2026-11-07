# Templates and test data

Payloads and handshake header values can hold placeholders that wsblast fills
in for every frame or every connection. With a data file, every connection
can send its own user id, token or channel, and in a scenario it can keep
values from replies, such as a session id, and check what every reply holds.

## 1. Placeholders

| Placeholder | Value | In payloads | In `-H` values |
| :--- | :--- | :--- | :--- |
| `{{seq}}` | The frame's number on its connection, from 0 | yes | always `0` |
| `{{timestamp}}` | Unix time in milliseconds, from the system clock | yes | yes, at the handshake |
| `{{worker_id}}` | The connection's number in the run, from 0 | yes | yes |
| `{{data.COLUMN}}` | The value of `COLUMN` in the connection's row of the data file | yes | yes |
| `{{NAME}}` | A variable the scenario phase captured from a reply ([section 3](#3-values-from-replies-and-reply-checks)) | scenario payloads | no |

Anything else in double braces, such as `{{name}}`, is not a placeholder and
is sent as written, so payloads that carry their own template syntax are not
changed. `{{timestamp}}` is never used for measurement: every latency comes
from a monotonic clock (METHODOLOGY.md, section 4).

A payload is parsed once, before the run. Each frame then copies the text
between the placeholders and formats their values, so a template costs
about as much to send as a fixed payload.

## 2. Data files

```bash
wsblast wss://api.example.com/ws -c 500 -d 2m --data users.csv \
  -H 'Authorization: Bearer {{data.token}}' \
  -p '{"op":"subscribe","user":"{{data.user_id}}","channel":"{{data.channel}}"}'
```

`--data FILE` reads a table:

- **CSV**: a header row with the column names, then one row per line. Fields
  are separated by commas; a field in double quotes may hold commas and line
  breaks, and `""` for a quote.
- **JSON Lines**: one object per line, with string, number or boolean
  values; the keys of the first line are the columns, and every line must
  have the same keys. A file is read as JSON Lines when its name ends in
  `.jsonl` or `.ndjson`, or when its first character that is not white
  space is `{`.

```csv
user_id,token,channel
u-1001,eyJhbGciOi...,prices
u-1002,eyJhbGciOi...,trades
```

Connection `n` uses row `n`, counting from 0. With more connections than
rows the rows repeat, row 0 again after the last one, and wsblast warns
before the run. A reconnecting connection keeps its row. Give the file at
least as many rows as connections when the server rejects a second session
for the same user.

Every `{{data.COLUMN}}` is checked before the run: a column the file does not
have fails with the file's columns, a template that reads a column without a
data file fails with what to add, and a header value that would not be valid
HTTP with some row, such as one with a line break, fails with the row number.
An error in the file itself names the line:

```text
Error: Configuration error: Invalid option: payload: {{data.email}}: users.csv has no column 'email' (columns: user_id, token, channel)
Error: Configuration error: Data file error: users.csv: line 7: 2 values for 3 columns
```

The file is read into memory, so a data file can have at most 256 MiB.

### In a scenario

A scenario names its data file with the top-level `data` key, relative to the
scenario file. `--data` on the command line replaces it. Every phase, and
every `Send` action, can read the columns:

```yaml
version: "1.0"
metadata:
  name: subscribers
data: users.csv
phases:
  - name: subscribe
    connections: 500
    duration: 2m
    actions:
      - type: Send
        payload: '{"op":"subscribe","user":"{{data.user_id}}"}'
      - type: Wait
        duration: 30s
```

### In a distributed run

The coordinator sends the rows to the agents with the rest of the
configuration, so the file only has to be on the coordinator. The agents
interleave their connections over the rows: with two agents, the first
uses rows 0, 2, 4 and so on and the second rows 1, 3, 5, so no two
machines use the same row while the file has enough of them.

## 3. Values from replies, and reply checks

Many WebSocket APIs work in steps: a login message returns a session, and
every later message carries it. A scenario `Send` action can keep values from
its replies with `capture` and check every reply with `expect`; both need
echo mode, where each message waits for its reply.

```yaml
version: "1.0"
metadata:
  name: login-then-subscribe
data: users.csv
phases:
  - name: session
    connections: 200
    duration: 2m
    actions:
      - type: Send
        payload: '{"op":"login","token":"{{data.token}}"}'
        expect: { json: /status, equals: ok }
        capture: { session: /session_id }
      - type: Send
        count: 10
        payload: '{"op":"subscribe","session":"{{session}}","id":{{seq}}}'
        expect: { json: /subscribed }
      - type: Wait
        duration: 5s
```

### `capture`

`capture` maps variable names to JSON pointers (RFC 6901). For every reply,
the value at each pointer is stored in its variable: a string as it is, any
other JSON value as JSON text, so `/count` holding `3` stores `3` and
`/user` holding an object stores the object. Any payload of the phase reads
it as `{{NAME}}`.

- Variables belong to one connection, and a reconnect clears them, as a new
  session would. Before the first capture a variable is empty.
- Names are letters, digits and `_`, starting with a letter or `_`; `seq`,
  `timestamp`, `worker_id` and `data` are taken.
- A `{{NAME}}` that no action of the phase captures is not a variable, and is
  sent as written.
- A reply that is not JSON, or has nothing at a pointer, fails the capture: it
  is a `check_failed` error, and the variable keeps its last value.

### `expect`

| Form | The reply passes when |
| :--- | :--- |
| `{ json: /pointer }` | It is JSON with a value, any value, at the pointer. |
| `{ json: /pointer, equals: VALUE }` | It is JSON and the value at the pointer equals `VALUE`. A number or boolean also equals its text, so `equals: "200"` matches `200`. |
| `{ contains: TEXT }` | Its text contains `TEXT`. |

`VALUE` and `TEXT` are compared as written; they are not templates.

Without a scenario, `--expect` checks every reply of the run with the same
forms, spelled `json:/pointer`, `json:/pointer=VALUE` and `contains:TEXT`.
`VALUE` is read as JSON when it is valid JSON and as a string otherwise, so
`json:/status=ok`, `json:/status="ok"` and `json:/code=200` all work. In a
scenario, `--expect` applies to every reply, on top of each action's own
`expect`.

```bash
wsblast wss://api.example.com/ws -c 100 -d 1m -p '{"op":"ping"}' --expect json:/op=pong
```

### How a failed check counts

A reply that fails a check, or a capture, counts one `check_failed` error,
however many of its checks failed. Its round-trip time is still recorded,
since it arrived, and like a reply timeout the error belongs to a message that
is already counted, so a run whose replies all fail has an error rate of 100%,
not more. `--max-error-rate` therefore gates on wrong replies as well as
missing ones. Replies to requests that chaos corrupted (`--chaos-corrupt`)
are not checked, because a wrong reply to a broken request is the injected
fault. See METHODOLOGY.md, section 6.1.
