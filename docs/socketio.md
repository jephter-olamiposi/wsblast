# Socket.IO

`--mode socketio` load-tests a Socket.IO server over WebSocket. Each
connection opens a Socket.IO session, connects to a namespace, and emits
every message as an event that asks for an acknowledgement. The time from
the event to its ack is the round-trip time, so everything echo mode
measures works the same way: RTT percentiles, response time with `--rate`,
latency SLOs, `--expect`, scenario actions with captures and named actions,
arrival rates and chaos.

It speaks Engine.IO protocol 4 with Socket.IO protocol 5, which Socket.IO 3
and later servers use. It is part of the default build (Cargo feature
`socketio`). `scripts/e2e.sh` checks it, text and binary, compressed and
not, against the socket.io 4 server, and the mock against the socket.io 4
client.

```bash
wsblast ws://localhost:3000 --mode socketio --sio-event chat \
  -p '{"room":"lobby","text":"hi {{seq}}"}' -c 200 -d 1m -r 5 --max-p99 50ms
```

The server's handler must call the acknowledgement callback, which is how
Socket.IO servers answer requests:

```js
io.on("connection", (socket) => {
  socket.on("chat", (message, ack) => {
    ack({ ok: true, seq: message.seq });
  });
});
```

## 1. Options

| Option | Default | Description |
| :--- | :--- | :--- |
| `--mode socketio` | | Socket.IO mode. |
| `--sio-event NAME` | `message` | The event every message is emitted as. The names Socket.IO reserves (`connect`, `connect_error`, `disconnect`, `disconnecting`, `newListener`, `removeListener`) are rejected. |
| `--sio-namespace NS` | `/` | The namespace to connect to, such as `/chat`. It must start with `/` and hold no `,`. |
| `--sio-auth JSON` | none | A JSON object sent with the namespace connect, as a Socket.IO client's `auth` option, such as `{"token":"abc"}`. Templates are not filled in; use `-H` with `{{data.COLUMN}}` for a per-connection credential in a header. |

Any other option works as in echo mode. `--match` keeps its default: the
reply to an event is its ack, matched by the ack id. With `--binary` (or a
binary payload in a scenario) every message is a binary event, as described
in [section 3.1](#31-binary-attachments).

## 2. The URL

When the target URL has no path, wsblast connects to `/socket.io/`, the
path Socket.IO servers listen on by default. It always adds
`EIO=4&transport=websocket` to the query, unless the URL already sets them,
and keeps any other query parameters:

| Given | Connected |
| :--- | :--- |
| `ws://localhost:3000` | `ws://localhost:3000/socket.io/?EIO=4&transport=websocket` |
| `wss://api.example.com/io/?token=x` | `wss://api.example.com/io/?token=x&EIO=4&transport=websocket` |

The report shows the URL it connected to. Socket.IO servers accept a
WebSocket connection without HTTP long-polling first, unless they were
configured to allow polling only.

## 3. A session

1. The WebSocket handshake. Its time is the handshake latency, as in every
   mode.
2. The server's Engine.IO open packet (`0{"sid":...}`), then the namespace
   connect (`40`, or `40/chat,` with the namespace, followed by the
   `--sio-auth` object) and the server's connect packet. A server that
   refuses the namespace (a connect error packet) or does not speak
   Engine.IO 4 is a `protocol_error`, and one that does not open the
   session within `--message-timeout` a `timeout`, each recorded once before
   any message is sent. The connection is then retried like a lost one.
3. The messages. Message `n` of the session is emitted as event `n` with
   ack id `n`: `42n["chat",PAYLOAD]`, or `42/chat,n[...]` in a namespace.
   A payload that is valid JSON is the event's argument as it is; any
   other text is sent as a JSON string. The ack (`43n[...]`) is the reply,
   and the reply's text, which `--expect` and `capture` read, is the ack's
   list of arguments: `--expect json:/0/ok=true` checks the first argument.
4. On the way, the server's Engine.IO pings (`2`) are answered with pongs
   (`3`) at once, even while the connection waits for its next send, so a
   slow `--rate` does not miss the server's ping timeout. Events the server
   emits on its own and acks of earlier events are counted as unsolicited
   frames and are not replies.
5. At the end, the connection leaves the namespace (`41`) and closes.

A message without an ack within `--message-timeout` is a `timeout` error,
as an echo reply would be. A server that disconnects the namespace (`41`)
or closes the Engine.IO session (`1`) ends the session like a WebSocket
close.

### 3.1 Binary attachments

A binary message is emitted as a binary event with one attachment: the
packet `451-n["chat",{"_placeholder":true,"num":0}]`, then the bytes in a
binary WebSocket frame, as a Socket.IO client sends `socket.emit("chat",
buffer, ack)`. The server's binary ack (`46k-n[...]`) is the reply once all
of its `k` attachments have arrived, each in a binary frame of its own:

- An ack whose only argument is one attachment, `ack(buffer)`, gives the
  attachment's bytes as the reply, so byte counts and RTT are those of an
  echo of the bytes.
- Any other binary ack gives its arguments as text, with each attachment's
  placeholder in its place, so `--expect json:/0/size=4096` can check an
  ack such as `ack({size: buffer.length})` that also carries a buffer.

A binary event the server emits on its own counts as one unsolicited frame
however many attachments it has. A packet that announces more than 1,024
attachments is not read as a packet.

## 4. Scenarios

Scenario actions run over the Socket.IO session: every `Send` is an event,
so a script can log in, keep a value from the login's ack and send it with
every later event:

```yaml
version: "1.0"
metadata: { name: chat }
phases:
  - name: chat
    connections: 100
    duration: 2m
    rate_per_conn: 2
    actions:
      - type: Send
        name: join
        payload: '{"op":"join","room":"lobby"}'
        capture: { member: /0/member_id }
      - type: Send
        name: say
        count: 10
        payload: '{"op":"say","member":"{{member}}","seq":{{seq}}}'
        expect: { json: /0/ok, equals: true }
```

```bash
wsblast --scenario chat.yaml --mode socketio --sio-event chat ws://localhost:3000
```

`Ping` actions send WebSocket pings, which Socket.IO servers ignore; the
Engine.IO pings are answered on their own.

## 5. Testing against the mock

`wsblast mock --socketio` speaks the server side: it accepts a connect to
any namespace, acknowledges every event that asks for an ack with the
event's arguments after `--delay` and `--jitter` (a binary event with its
attachments, as a binary ack), echoes events that do not,
and pings every 25 seconds, closing a connection that does not answer
within 20, as a Socket.IO server does. Stalls and `--reject-ratio` apply as
usual; it cannot be combined with `--rules` or `--replay`.

```bash
wsblast mock --socketio --delay 5ms &
wsblast ws://127.0.0.1:9001 --mode socketio -c 50 -d 30s -r 10
```

## 6. Limits

- A binary message is one attachment in an event of one argument; events
  that mix JSON arguments and several attachments come only from the
  server.
- Only the WebSocket transport is used, and only Engine.IO 4: Socket.IO 2
  servers (Engine.IO 3) are not supported.
- Each connection has one event in flight at a time, as in echo mode.
- A distributed run sends the `--sio-*` settings to every agent; the agents
  need the `socketio` feature, which default builds have.
