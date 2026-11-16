import { rmSync } from "node:fs";

// Windows can keep a just-exited program's executable, or a folder it ran in,
// locked for a moment, so removing a test's scratch folder right after its
// child exits fails with EBUSY or EPERM. Bun's rmSync gives up on the first
// such error even when Node's `maxRetries` option is set, so retry here.
const RETRYABLE = new Set(["EBUSY", "EPERM", "ENOTEMPTY"]);

/** Remove a scratch folder, retrying lock errors for up to 10 seconds. */
export async function removeScratch(dir: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  for (;;) {
    try {
      rmSync(dir, { recursive: true, force: true });
      return;
    } catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      if (!RETRYABLE.has(code ?? "") || Date.now() >= deadline) throw error;
      await Bun.sleep(100);
    }
  }
}
