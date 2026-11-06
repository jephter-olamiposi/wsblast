// Paced load: each VU keeps one WebSocket and sends RATE messages a second
// on a timer, whether or not earlier replies have arrived. Each message
// carries its send time, and the reply's age is recorded as ws_rtt.
//   k6 run -e URL=ws://127.0.0.1:9001 -e VUS=10 -e RATE=20 -e DURATION=30s k6-paced.js
import { WebSocket } from 'k6/websockets';
import { Trend } from 'k6/metrics';

const rtt = new Trend('ws_rtt', true);
const vus = Number(__ENV.VUS || 10);
const rate = Number(__ENV.RATE || 1);

export const options = {
  scenarios: {
    paced: {
      executor: 'per-vu-iterations',
      vus,
      iterations: 1,
      maxDuration: '10m',
    },
  },
  summaryTrendStats: ['p(50)', 'p(99)', 'max'],
};

const seconds = parseInt(__ENV.DURATION || '30s', 10);

export default function () {
  const ws = new WebSocket(__ENV.URL);
  ws.onopen = () => {
    const timer = setInterval(() => ws.send(String(Date.now())), 1000 / rate);
    setTimeout(() => {
      clearInterval(timer);
      ws.close();
    }, seconds * 1000);
  };
  ws.onmessage = (e) => rtt.add(Date.now() - Number(e.data));
}
