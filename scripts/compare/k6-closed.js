// Closed loop: each VU sends a message, waits for its reply, records the
// time, then waits for the rest of the 1/RATE interval before the next one.
// This is the common request/response script shape.
//   k6 run -e URL=ws://127.0.0.1:9001 -e VUS=10 -e RATE=20 -e DURATION=30s k6-closed.js
import { WebSocket } from 'k6/websockets';
import { Trend } from 'k6/metrics';

const rtt = new Trend('ws_rtt', true);
const vus = Number(__ENV.VUS || 10);
const rate = Number(__ENV.RATE || 1);
const seconds = parseInt(__ENV.DURATION || '30s', 10);

export const options = {
  scenarios: {
    closed: { executor: 'per-vu-iterations', vus, iterations: 1, maxDuration: '10m' },
  },
  summaryTrendStats: ['p(50)', 'p(99)', 'max'],
};

export default function () {
  const ws = new WebSocket(__ENV.URL);
  const interval = 1000 / rate;
  let sentAt = 0;
  let stop = false;
  const sendNext = () => {
    if (stop) return;
    sentAt = Date.now();
    ws.send('hello');
  };
  ws.onopen = () => {
    setTimeout(() => {
      stop = true;
      ws.close();
    }, seconds * 1000);
    sendNext();
  };
  ws.onmessage = () => {
    const took = Date.now() - sentAt;
    rtt.add(took);
    setTimeout(sendNext, Math.max(0, interval - took));
  };
}
