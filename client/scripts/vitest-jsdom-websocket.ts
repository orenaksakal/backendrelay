import WebSocket from 'ws';

if (typeof window !== 'undefined') {
  Object.defineProperty(window.navigator, 'onLine', {
    configurable: true,
    value: true,
  });
  Object.defineProperty(globalThis, 'WebSocket', {
    configurable: true,
    value: WebSocket,
    writable: true,
  });
  Object.defineProperty(window, 'WebSocket', {
    configurable: true,
    value: WebSocket,
    writable: true,
  });
}
