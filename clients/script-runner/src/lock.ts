import { isBuiltin } from "node:module";
import { plugin } from "bun";
import { literalRequires } from "./warnings.ts";

export function allowed(specifier: string): boolean {
  return specifier === "ck:registration" || specifier === "bun" ||
    specifier.startsWith("bun:") || isBuiltin(specifier);
}

export function check(source: string): string[] {
  const imports = new Bun.Transpiler({ loader: "js", target: "bun" }).scanImports(source);
  // Bun bundles can alias import.meta.require; scanImports omits those calls.
  return [...new Set([...imports.map(item => item.path), ...literalRequires(source)].filter(path => !allowed(path)))];
}

export async function evaluate(source: string, helper: Record<string, unknown>): Promise<void> {
  // Check static ESM specifiers before Bun tries package lookup or any evaluation.
  const imports = new Bun.Transpiler({ loader: "js", target: "bun" }).scanImports(source);
  for (const item of imports) {
    if (item.kind === "import-statement" && !allowed(item.path)) throw new Error(`script_import_refused: ${item.path}`);
  }
  plugin({
    name: "checked-script",
    setup(build) {
      // Imports from a virtual module can resolve in the ordinary file namespace.
      for (const namespace of ["file", "ck-script", "ck"]) {
        build.onResolve({ filter: /.*/, namespace }, ({ path }) => {
          if (namespace === "ck-script" && path === "main") return { path, namespace: "ck-script" };
          if ((namespace === "ck" && path === "registration") || path === "ck:registration") {
            return { path: "registration", namespace: "ck" };
          }
          if (allowed(path)) return { path, external: true };
          throw new Error(`script_import_refused: ${path}`);
        });
      }
      build.onLoad({ filter: /^registration$/, namespace: "ck" }, () => ({ exports: helper, loader: "object" }));
      build.onLoad({ filter: /^main$/, namespace: "ck-script" }, () => ({
        // An explicit export prevents Bun from classifying require-only input as CJS.
        contents: `Object.defineProperties(import.meta, {url: {value: "ck-script:main"}, dir: {value: undefined}, dirname: {value: undefined}});\n${source}\n;export {};`, loader: "js",
      }));
    },
  });
  const main = "ck-script:main";
  try { await import(main); }
  catch (error) {
    // Bun may reject an absent bare package before calling a file-namespace hook.
    const code = (error as NodeJS.ErrnoException).code;
    if (code === "MODULE_NOT_FOUND" || code === "ERR_MODULE_NOT_FOUND") {
      throw new Error(`script_import_refused: ${(error as Error).message}`);
    }
    throw error;
  }
}
