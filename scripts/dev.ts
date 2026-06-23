// Runs the Rust backend and the Svelte frontend together.
// Usage:  bun dev   (from the repo root)
// Ctrl-C stops both. Output from each is prefixed so it's easy to tell apart.
import { spawn } from "bun";

const root = new URL("..", import.meta.url).pathname;

type Svc = { name: string; cmd: string[]; cwd: string; color: string };

const services: Svc[] = [
  { name: "backend ", cmd: ["cargo", "run"], cwd: `${root}backend`, color: "\x1b[38;5;208m" },
  { name: "frontend", cmd: ["bun", "run", "dev"], cwd: `${root}frontend`, color: "\x1b[38;5;141m" },
];

const reset = "\x1b[0m";

function pipe(svc: Svc, stream: ReadableStream<Uint8Array>) {
  const reader = stream.getReader();
  const dec = new TextDecoder();
  let buf = "";
  (async () => {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      const lines = buf.split("\n");
      buf = lines.pop() ?? "";
      for (const line of lines) {
        console.log(`${svc.color}[${svc.name}]${reset} ${line}`);
      }
    }
  })();
}

const procs = services.map((svc) => {
  const p = spawn(svc.cmd, { cwd: svc.cwd, stdout: "pipe", stderr: "pipe" });
  pipe(svc, p.stdout);
  pipe(svc, p.stderr);
  return p;
});

function shutdown() {
  for (const p of procs) p.kill();
  process.exit(0);
}
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

console.log("⟁ Optimimer dev — backend :8799 · frontend :5173 (Ctrl-C to stop)\n");
await Promise.all(procs.map((p) => p.exited));
