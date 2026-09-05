# @cortexkit/log

The TypeScript fleet logger for harness-hosted CortexKit plugins. It writes the canonical line format and module-owned files specified in `docs/specs/fleet-logging.md`.

```ts
import { createLogger } from "@cortexkit/log";

const log = createLogger({
  moduleId: "magic-context",
  lane: { kind: "plugin", harness: "opencode" },
  tags: ["perf"],
});

log.info("plugin started");
```

`CK_LOG` controls the default level and declared tag overrides. Retention defaults to 32 MiB per file, two generations, and fourteen days. Each event is written synchronously as one line, so the package does not rely on a timer to flush pending lines before a harness exits.
