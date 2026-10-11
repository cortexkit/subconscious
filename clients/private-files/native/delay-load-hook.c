#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <delayimp.h>
#include <string.h>

/* node.lib names node.exe, but Bun hosts the same N-API symbols in bun.exe.
 * Resolve delay-loaded node.exe imports against the running executable instead
 * of loading a second, uninitialized Node image into Bun's process. */
static FARPROC WINAPI host_napi_import(unsigned notification, PDelayLoadInfo info) {
  if (notification != dliNotePreLoadLibrary || _stricmp(info->szDll, "node.exe")) return NULL;
  return (FARPROC)GetModuleHandleW(NULL);
}
const PfnDliHook __pfnDliNotifyHook2 = host_napi_import;
