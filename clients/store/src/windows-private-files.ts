type PrivateFiles = typeof import("@cortexkit/private-files");

/** Load only for Windows operations so Unix consumers never need a native addon. */
export async function withWindowsPrivateFiles<T>(operation: (api: PrivateFiles) => Promise<T>): Promise<T> {
  let api: PrivateFiles;
  try {
    api = await import("@cortexkit/private-files");
  } catch (cause) {
    throw new Error("@cortexkit/private-files is unavailable; refusing the Windows private operation", { cause });
  }
  try {
    return await operation(api);
  } catch (cause) {
    if (cause instanceof api.PrivateFilesUnavailableError) {
      throw new Error(`@cortexkit/private-files native addon is unavailable; refusing the Windows private operation: ${cause.message}`, { cause });
    }
    throw cause;
  }
}
