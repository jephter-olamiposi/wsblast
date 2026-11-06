// Unpaced echo: each VU keeps one WebSocket and sends the next message as
// soon as the previous reply arrives, for DURATION seconds.
//   k6 run -e URL=ws://127.0.0.1:9001 -e VUS=50 -e DURATION=20s k6-echo.js
import { WebSocket } from 'k6/websockets';

const vus = Number(__ENV.VUS || 50);
const seconds = parseInt(__ENV.DURATION || '20s', 10);

export const options = {
  scenarios: {
    echo: { executor: 'per-vu-iterations', vus, iterations: 1, maxDuration: '10m' },
  },
};

export default function () {
  const ws = new WebSocket(__ENV.URL);
  let stop = false;
  ws.onopen = () => {
    setTimeout(() => {
      stop = true;
      ws.close();
    }, seconds * 1000);
    ws.send('hello');
  };
  ws.onmessage = () => {
    if (!stop) ws.send('hello');
  };
}
