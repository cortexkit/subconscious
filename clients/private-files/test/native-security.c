/* Fault injection exercises the addon itself. Filesystem calls are real except
 * at explicit race/failure boundaries; N-API calls use a deterministic fake host. */
#define WIN32_LEAN_AND_MEAN
#define _WIN32_WINNT 0x0602
#define NAPI_VERSION 8
#define NAPI_EXTERN
#include <windows.h>
#include <aclapi.h>
#include <winternl.h>
#include <node_api.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <wchar.h>
#include <stddef.h>

static int failures;
#define REQUIRE(condition) do { if (!(condition)) { fprintf(stderr, "%s:%d: %s\n", __FILE__, __LINE__, #condition); ++failures; } } while (0)
static int calls, fail_at, fail_forever, resolved, rejected, deleted, queued, thrown, invalid_value;
static napi_status injected_status = napi_generic_failure;
static void *queued_data;
static int secret_frees, secret_dirty, track_secret, fail_owner_allocation;
static void *secret_ptr;
static size_t secret_size = 17;
static wchar_t api_path[1024];
static BYTE api_bytes[17] = "secret copy data";

static napi_status next_call(void) {
  ++calls;
  return fail_at && (calls == fail_at || (fail_forever && calls >= fail_at)) ? injected_status : napi_ok;
}
static napi_status output(napi_value *value) {
  napi_status status = next_call();
  if (status == napi_ok) *value = (napi_value)(uintptr_t)(calls + 16);
  return status;
}
napi_status napi_create_string_utf8(napi_env env, const char *text, size_t n, napi_value *value) { return output(value); }
napi_status napi_create_string_utf16(napi_env env, const char16_t *text, size_t n, napi_value *value) { return output(value); }
napi_status napi_create_error(napi_env env, napi_value code, napi_value message, napi_value *value) { if (!message) ++invalid_value; return output(value); }
napi_status napi_create_uint32(napi_env env, uint32_t n, napi_value *value) { return output(value); }
napi_status napi_get_undefined(napi_env env, napi_value *value) { return output(value); }
napi_status napi_create_buffer_copy(napi_env env, size_t n, const void *data, void **copy, napi_value *value) { return output(value); }
napi_status napi_set_named_property(napi_env env, napi_value object, const char *name, napi_value value) { if (!object || !value) ++invalid_value; return next_call(); }
napi_status napi_resolve_deferred(napi_env env, napi_deferred deferred, napi_value value) { if (!value || !deferred) ++invalid_value; napi_status s = next_call(); if (s == napi_ok) ++resolved; return s; }
napi_status napi_reject_deferred(napi_env env, napi_deferred deferred, napi_value value) { if (!value || !deferred) ++invalid_value; napi_status s = next_call(); if (s == napi_ok) ++rejected; return s; }
napi_status napi_get_and_clear_last_exception(napi_env env, napi_value *value) { return output(value); }
napi_status napi_delete_async_work(node_api_basic_env env, napi_async_work work) { ++deleted; return next_call(); }
napi_status napi_throw_type_error(napi_env env, const char *code, const char *message) { ++thrown; return next_call(); }
napi_status napi_throw_error(napi_env env, const char *code, const char *message) { ++thrown; return next_call(); }
napi_status napi_get_cb_info(napi_env env, napi_callback_info info, size_t *argc, napi_value *args, napi_value *self, void **data) {
  napi_status s = next_call(); if (s == napi_ok) { *argc = 3; args[0] = (napi_value)1; args[1] = (napi_value)2; args[2] = (napi_value)3; *data = NULL; } return s;
}
napi_status napi_get_value_string_utf16(napi_env env, napi_value value, char16_t *buffer, size_t capacity, size_t *n) {
  napi_status s = next_call(); if (s == napi_ok) { *n = wcslen(api_path); if (buffer) memcpy(buffer, api_path, (*n + 1) * sizeof(wchar_t)); } return s;
}
napi_status napi_get_value_string_utf8(napi_env env, napi_value value, char *buffer, size_t capacity, size_t *n) {
  napi_status s = next_call(); if (s == napi_ok) { strcpy(buffer, "file"); *n = 4; } return s;
}
napi_status napi_get_typedarray_info(napi_env env, napi_value value, napi_typedarray_type *type, size_t *n, void **data, napi_value *buffer, size_t *offset) {
  napi_status s = next_call(); if (s == napi_ok) { *type = napi_uint8_array; *n = sizeof(api_bytes); *data = api_bytes; *buffer = (napi_value)4; *offset = 0; } return s;
}
napi_status napi_create_async_work(napi_env env, napi_value resource, napi_value name, napi_async_execute_callback execute, napi_async_complete_callback complete, void *data, napi_async_work *work) {
  if (!name) ++invalid_value;
  napi_status s = next_call(); if (s == napi_ok) { *work = (napi_async_work)1; queued_data = data; } return s;
}
napi_status napi_create_promise(napi_env env, napi_deferred *deferred, napi_value *promise) { napi_status s = output(promise); if (s == napi_ok) *deferred = (napi_deferred)1; return s; }
napi_status napi_queue_async_work(node_api_basic_env env, napi_async_work work) { napi_status s = next_call(); if (s == napi_ok) ++queued; return s; }
napi_status napi_create_function(napi_env env, const char *name, size_t n, napi_callback callback, void *data, napi_value *value) { return output(value); }

static void *probe_malloc(size_t n) {
  if (fail_owner_allocation) { fail_owner_allocation = 0; return NULL; }
  void *p = malloc(n);
  if (track_secret && n == secret_size) secret_ptr = p;
  return p;
}
static void probe_free(void *p) {
  if (p && p == secret_ptr) {
    for (size_t i = 0; i < secret_size; ++i) if (((BYTE *)p)[i]) { ++secret_dirty; break; }
    ++secret_frees; secret_ptr = NULL;
  }
  free(p);
}

static int race_enabled, race_moved, force_kind_failure, share_probe, reader_opened, flush_calls, flush_failure, write_failure, opens;
static wchar_t race_path[1024], moved_path[1024];
static HANDLE created_handle = INVALID_HANDLE_VALUE;
static void attack_directory(void) {
  race_moved = MoveFileExW(race_path, moved_path, 0);
  if (race_moved) REQUIRE(CreateDirectoryW(race_path, NULL));
}
static BOOL WINAPI probe_CreateDirectoryW(LPCWSTR path, LPSECURITY_ATTRIBUTES sa) {
  BOOL result = CreateDirectoryW(path, sa);
  if (result && race_enabled) attack_directory();
  return result;
}
static NTSTATUS NTAPI probe_NtCreateFile(PHANDLE h, ACCESS_MASK access, POBJECT_ATTRIBUTES object, PIO_STATUS_BLOCK io, PLARGE_INTEGER size,
    ULONG attributes, ULONG share, ULONG disposition, ULONG options, PVOID ea, ULONG ea_size) {
  ++opens;
  NTSTATUS result = NtCreateFile(h, access, object, io, size, attributes, share, disposition, options, ea, ea_size);
  if (result >= 0 && disposition == FILE_CREATE && (options & FILE_DIRECTORY_FILE)) {
    created_handle = *h;
    if (race_enabled) attack_directory();
  }
  return result;
}
static HANDLE WINAPI probe_CreateFileW(LPCWSTR path, DWORD access, DWORD share, LPSECURITY_ATTRIBUTES sa, DWORD disposition, DWORD flags, HANDLE template) {
  ++opens;
  HANDLE h = CreateFileW(path, access, share, sa, disposition, flags, template);
  if (race_enabled && !wcscmp(path, race_path)) created_handle = h;
  return h;
}
static DWORD WINAPI probe_GetFileType(HANDLE h) {
  if (force_kind_failure && h == created_handle) return FILE_TYPE_CHAR;
  return GetFileType(h);
}
static BOOL WINAPI probe_WriteFile(HANDLE h, LPCVOID bytes, DWORD length, LPDWORD written, LPOVERLAPPED overlap) {
  if (write_failure) { SetLastError(ERROR_WRITE_FAULT); return FALSE; }
  if (share_probe) {
    HANDLE reader = CreateFileW(api_path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING, 0, NULL);
    if (reader != INVALID_HANDLE_VALUE) { ++reader_opened; CloseHandle(reader); }
    else REQUIRE(GetLastError() == ERROR_SHARING_VIOLATION);
  }
  return WriteFile(h, bytes, length, written, overlap);
}
static BOOL WINAPI probe_FlushFileBuffers(HANDLE h) {
  ++flush_calls;
  if (flush_failure) { SetLastError(ERROR_WRITE_FAULT); return FALSE; }
  return FlushFileBuffers(h);
}

static int token_mode, token_sizing_calls;
static TOKEN_INFORMATION_CLASS token_target = TokenUser;
static BOOL WINAPI probe_GetTokenInformation(HANDLE h, TOKEN_INFORMATION_CLASS type, LPVOID buffer, DWORD capacity, PDWORD size) {
  if (!buffer) {
    ++token_sizing_calls;
    if (token_mode && token_mode != 5 && type == token_target) {
      *size = token_mode == 3 ? 1 : 0;
      SetLastError(token_mode == 1 ? ERROR_ACCESS_DENIED : ERROR_INSUFFICIENT_BUFFER);
      return token_mode == 4;
    }
  }
  BOOL result = GetTokenInformation(h, type, buffer, capacity, size);
  if (type == TokenOwner && !buffer && token_mode == 5) fail_owner_allocation = 1;
  return result;
}

static PSID fake_owner;
static PSID fake_user;
static int fake_descriptor;
static DWORD fake_mask = FILE_ALL_ACCESS;
static BYTE fake_flags, fake_type;
static int fake_null_dacl;
static DWORD WINAPI probe_GetSecurityInfo(HANDLE h, SE_OBJECT_TYPE type, SECURITY_INFORMATION information,
    PSID *owner, PSID *group, PACL *acl, PACL *sacl, PSECURITY_DESCRIPTOR *sd) {
  if (!fake_descriptor) return GetSecurityInfo(h, type, information, owner, group, acl, sacl, sd);
  DWORD length = sizeof(ACL) + sizeof(ACCESS_ALLOWED_ACE) - sizeof(DWORD) + GetLengthSid(fake_user);
  BYTE *block = LocalAlloc(LPTR, SECURITY_DESCRIPTOR_MIN_LENGTH + length);
  *sd = block; *acl = (PACL)(block + SECURITY_DESCRIPTOR_MIN_LENGTH); *owner = fake_owner;
  InitializeSecurityDescriptor(*sd, SECURITY_DESCRIPTOR_REVISION);
  InitializeAcl(*acl, length, ACL_REVISION);
  AddAccessAllowedAceEx(*acl, ACL_REVISION, fake_flags, fake_mask, fake_user);
  ((ACE_HEADER *)((BYTE *)*acl + sizeof(ACL)))->AceType = fake_type;
  if (fake_null_dacl) *acl = NULL;
  SetSecurityDescriptorDacl(*sd, TRUE, *acl, FALSE);
  SetSecurityDescriptorControl(*sd, SE_DACL_PROTECTED, SE_DACL_PROTECTED);
  return ERROR_SUCCESS;
}
static int malformed, sid_calls;
static ACE_HEADER *fake_ace;
static BOOL WINAPI probe_IsValidAcl(PACL acl) { return malformed ? TRUE : IsValidAcl(acl); }
static BOOL WINAPI probe_GetAce(PACL acl, DWORD index, LPVOID *ace) { if (malformed) { *ace = fake_ace; return TRUE; } return GetAce(acl, index, ace); }
static BOOL WINAPI probe_IsValidSid(PSID sid) { if (malformed) ++sid_calls; return IsValidSid(sid); }

#define malloc probe_malloc
#define free probe_free
#define CreateDirectoryW probe_CreateDirectoryW
#define CreateFileW probe_CreateFileW
#define NtCreateFile probe_NtCreateFile
#define GetFileType probe_GetFileType
#define WriteFile probe_WriteFile
#define FlushFileBuffers probe_FlushFileBuffers
#define GetTokenInformation probe_GetTokenInformation
#define GetSecurityInfo probe_GetSecurityInfo
#define IsValidAcl probe_IsValidAcl
#define GetAce probe_GetAce
#define IsValidSid probe_IsValidSid
#include "../native/addon.c"
#undef malloc
#undef free
#undef CreateDirectoryW
#undef CreateFileW
#undef NtCreateFile
#undef GetFileType
#undef WriteFile
#undef FlushFileBuffers
#undef GetTokenInformation
#undef GetSecurityInfo
#undef IsValidAcl
#undef GetAce
#undef IsValidSid
/* MinGW declares the delay hook pointer mutable; the production MSVC header
 * declares it const. Only this cross-compile test adapts that declaration. */
#ifdef __MINGW32__
#define const
#endif
#include "../native/delay-load-hook.c"
#ifdef __MINGW32__
#undef const
#endif

static void safe_parent_aces(void) {
  Job identity = {0};
  REQUIRE(user_sid(&identity));
  fake_descriptor = 1;
  fake_owner = identity.sid;
  REQUIRE(ConvertStringSidToSidW(L"S-1-15-3-65536-599108337-2355189375-1353122160-3480128286-3345335107-485756383-4087318168-230526575", &fake_user));
  const DWORD masks[] = { FILE_TRAVERSE | SYNCHRONIZE, FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES,
    FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_DELETE_CHILD, DELETE, WRITE_DAC, WRITE_OWNER, GENERIC_WRITE, GENERIC_ALL };
  for (size_t i = 0; i < sizeof(masks) / sizeof(masks[0]); ++i) {
    Job j = {0}; j.sid = identity.sid;
    fake_mask = masks[i];
    REQUIRE(safe_descriptor(&j, NULL, safe_parent_rights(1, 1)) == (i < 2));
    if (i >= 2) REQUIRE(j.refusal && !strcmp(j.refusal, "writable-by-others"));
    if (j.detail) LocalFree(j.detail);
  }
  for (int variant = 0; variant < 4; ++variant) {
    Job j = {0}; j.sid = identity.sid;
    fake_mask = GENERIC_ALL;
    fake_flags = variant == 0 || variant == 2 ? INHERIT_ONLY_ACE : 0;
    fake_type = variant == 1 ? ACCESS_DENIED_ACE_TYPE : variant == 2 ? ACCESS_ALLOWED_CALLBACK_ACE_TYPE : 0;
    fake_null_dacl = variant == 3;
    REQUIRE(safe_descriptor(&j, NULL, safe_parent_rights(1, 1)) == (variant < 2));
    if (variant == 2) REQUIRE(j.refusal && !strcmp(j.refusal, "extra-ace"));
    if (variant == 3) REQUIRE(j.refusal && !strcmp(j.refusal, "null-dacl"));
    if (j.detail) LocalFree(j.detail);
  }
  fake_descriptor = fake_flags = fake_type = fake_null_dacl = 0;
  fake_mask = FILE_ALL_ACCESS;
  LocalFree(fake_user); fake_user = fake_owner = NULL;
  free(identity.token); free(identity.owner_token);
}

static void reset_host(void) {
  calls = fail_at = fail_forever = resolved = rejected = deleted = queued = thrown = invalid_value = 0;
  injected_status = napi_generic_failure;
  queued_data = NULL;
}
static Job *job(const wchar_t *path, int op, int directory) {
  Job *j = calloc(1, sizeof(Job));
  j->path = _wcsdup(path); j->leaf = INVALID_HANDLE_VALUE;
  j->op = op; j->directory = directory;
  j->work = (napi_async_work)1; j->deferred = (napi_deferred)1;
  return j;
}
static void run_create(const wchar_t *path, int directory) {
  Job *j = job(path, 0, directory);
  if (!directory) { j->length = sizeof(api_bytes); j->bytes = probe_malloc(j->length); memcpy(j->bytes, api_bytes, j->length); }
  execute(NULL, j);
  REQUIRE(!j->reason);
  release_job(j);
}
static void directory_identity(const wchar_t *root) {
  swprintf(race_path, 1024, L"%ls\\identity", root);
  swprintf(moved_path, 1024, L"%ls\\substituted-original", root);
  REQUIRE(CreateDirectoryW(race_path, NULL));
  attack_directory();
  REQUIRE(race_moved);
  RemoveDirectoryW(race_path); RemoveDirectoryW(moved_path);
  race_enabled = 1; race_moved = 0;
  run_create(race_path, 1);
  REQUIRE(!race_moved);
  DWORD attributes = GetFileAttributesW(race_path);
  REQUIRE(attributes != INVALID_FILE_ATTRIBUTES && (attributes & FILE_ATTRIBUTE_DIRECTORY));
  RemoveDirectoryW(race_path); RemoveDirectoryW(moved_path);
  force_kind_failure = 1; race_moved = 0; created_handle = INVALID_HANDLE_VALUE;
  Job *j = job(race_path, 0, 1);
  execute(NULL, j);
  REQUIRE(j->reason != NULL);
  REQUIRE(j->created && j->leaf != INVALID_HANDLE_VALUE);
  REQUIRE(!race_moved);
  REQUIRE(GetFileAttributesW(race_path) == INVALID_FILE_ATTRIBUTES);
  release_job(j);
  force_kind_failure = race_enabled = 0;
  RemoveDirectoryW(race_path); RemoveDirectoryW(moved_path);
}
static void private_write(const wchar_t *root) {
  swprintf(api_path, 1024, L"%ls\\write", root);
  share_probe = 1; reader_opened = flush_calls = 0;
  run_create(api_path, 0);
  REQUIRE(reader_opened == 0);
  REQUIRE(flush_calls == 1);
  HANDLE h = CreateFileW(api_path, GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, 0, NULL);
  BYTE bytes[17]; DWORD received = 0;
  REQUIRE(h != INVALID_HANDLE_VALUE && ReadFile(h, bytes, sizeof(bytes), &received, NULL));
  REQUIRE(received == sizeof(bytes) && !memcmp(bytes, api_bytes, sizeof(bytes)));
  CloseHandle(h); DeleteFileW(api_path);
  flush_failure = 1;
  Job *j = job(api_path, 0, 0); j->bytes = probe_malloc(sizeof(api_bytes)); j->length = sizeof(api_bytes); memcpy(j->bytes, api_bytes, j->length);
  execute(NULL, j);
  REQUIRE(j->error == ERROR_WRITE_FAULT);
  REQUIRE(GetFileAttributesW(api_path) == INVALID_FILE_ATTRIBUTES);
  release_job(j);
  share_probe = flush_failure = 0;
  DeleteFileW(api_path);
}
static void owner_policy(void) {
  Job j = {0}; REQUIRE(user_sid(&j));
  BYTE admins[SECURITY_MAX_SID_SIZE], system[SECURITY_MAX_SID_SIZE], users[SECURITY_MAX_SID_SIZE];
  DWORD n = sizeof(admins); REQUIRE(CreateWellKnownSid(WinBuiltinAdministratorsSid, NULL, admins, &n));
  n = sizeof(system); REQUIRE(CreateWellKnownSid(WinLocalSystemSid, NULL, system, &n));
  n = sizeof(users); REQUIRE(CreateWellKnownSid(WinBuiltinUsersSid, NULL, users, &n));
  fake_descriptor = 1; fake_user = j.sid; j.default_owner = users;
  PSID owners[] = { j.sid, admins, system, users };
  for (size_t i = 0; i < 4; ++i) {
    for (int canonical = 0; canonical <= 1; ++canonical) {
      fake_owner = owners[i]; j.reason = j.refusal = NULL;
      REQUIRE(descriptor(&j, NULL, canonical) == (i != 3));
      if (i == 3) REQUIRE(j.refusal && !strcmp(j.refusal, "wrong-owner"));
      if (j.detail) { LocalFree(j.detail); j.detail = NULL; }
    }
  }
  fake_descriptor = 0;
  free(j.token); free(j.owner_token);
}
static void napi_failures(void) {
  /* Inject a failure at each construction and settlement call, including calls
   * that build errors with a SID. */
  for (int error = 0; error < 2; ++error) {
    for (int step = 1; step <= (error ? 15 : 2); ++step) {
      reset_host(); fail_at = step;
      Job *j = job(L"C:\\token", 1, 0); j->length = sizeof(api_bytes); j->bytes = probe_malloc(j->length); memcpy(j->bytes, api_bytes, j->length);
      if (error) { j->reason = "Refused"; j->detail = LocalAlloc(LPTR, 32); wcscpy(j->detail, L"S-1-5"); }
      complete(NULL, napi_ok, j);
      REQUIRE(!invalid_value && !resolved && rejected == 1 && deleted == 1);
    }
  }
  reset_host(); fail_at = 1; fail_forever = 1;
  complete(NULL, napi_ok, job(L"C:\\token", 1, 0));
  REQUIRE(!resolved && !rejected && !invalid_value && deleted == 1);
  napi_status unavailable[] = { napi_closing, napi_cannot_run_js };
  for (size_t i = 0; i < 2; ++i) {
    reset_host(); complete(NULL, unavailable[i], job(L"C:\\token", 1, 0));
    REQUIRE(!resolved && !rejected && calls == 1 && deleted == 1);
    reset_host(); fail_at = 1; injected_status = unavailable[i];
    complete(NULL, napi_ok, job(L"C:\\token", 1, 0));
    REQUIRE(!resolved && !rejected && calls == 2);
  }
  reset_host(); fail_at = 1; injected_status = napi_pending_exception;
  complete(NULL, napi_ok, job(L"C:\\token", 1, 0));
  REQUIRE(rejected == 1 && !resolved && !invalid_value);
  wcscpy(api_path, L"C:\\token");
  for (int step = 1; step <= 9; ++step) {
    reset_host(); fail_at = step;
    napi_value result = dispatch(NULL, NULL);
    REQUIRE(!invalid_value && !queued);
    if (step == 9) REQUIRE(result && rejected == 1 && deleted == 1);
    else REQUIRE(!result && thrown == 1);
  }
  reset_host();
  REQUIRE(dispatch(NULL, NULL) != NULL && queued == 1);
  release_job(queued_data);
  for (int step = 1; step <= 8; ++step) {
    reset_host(); fail_at = step;
    REQUIRE(init(NULL, (napi_value)1) == NULL && !invalid_value && thrown == 1);
  }
}
static void zeroize(const wchar_t *root) {
  track_secret = 1; secret_dirty = secret_frees = 0;
  swprintf(api_path, 1024, L"%ls\\zero", root);
  run_create(api_path, 0);
  REQUIRE(secret_frees == 1 && !secret_dirty);
  reset_host(); Job *j = job(api_path, 1, 0); execute(NULL, j); REQUIRE(!j->reason); complete(NULL, napi_ok, j);
  REQUIRE(secret_frees == 2 && !secret_dirty);
  reset_host(); j = job(api_path, 1, 0); execute(NULL, j); fail_at = 1; complete(NULL, napi_ok, j);
  REQUIRE(secret_frees == 3 && !secret_dirty);
  reset_host(); fail_at = 6; dispatch(NULL, NULL);
  REQUIRE(secret_frees == 4 && !secret_dirty);
  write_failure = 1; DeleteFileW(api_path);
  j = job(api_path, 0, 0); j->length = sizeof(api_bytes); j->bytes = probe_malloc(j->length); memcpy(j->bytes, api_bytes, j->length);
  execute(NULL, j); REQUIRE(j->reason != NULL); release_job(j);
  REQUIRE(secret_frees == 5 && !secret_dirty);
  write_failure = track_secret = 0;
  DeleteFileW(api_path);
}
static void token_queries(void) {
  TOKEN_INFORMATION_CLASS targets[] = { TokenUser, TokenOwner };
  for (size_t i = 0; i < 2; ++i) {
    token_target = targets[i];
    for (int mode = 1; mode <= 4; ++mode) {
      Job j = {0}; token_mode = mode; token_sizing_calls = 0;
      REQUIRE(!user_sid(&j));
      REQUIRE(j.error == ERROR_INVALID_DATA);
      REQUIRE(token_sizing_calls >= 1);
      free(j.token); free(j.owner_token);
    }
  }
  token_mode = 5;
  Job allocation = {0}; REQUIRE(!user_sid(&allocation)); REQUIRE(allocation.error == ERROR_OUTOFMEMORY);
  free(allocation.token); free(allocation.owner_token);
  token_mode = 0;
  Job j = {0}; REQUIRE(user_sid(&j)); REQUIRE(j.sid && IsValidSid(j.sid)); free(j.token); free(j.owner_token);
}
static void reserved_names(const wchar_t *root) {
  const wchar_t *names[] = { L"con", L"PrN.txt", L"aUX", L"NUL.bin", L"conin$.log", L"CONOUT$" };
  for (int i = 0; i < 26; ++i) {
    wchar_t name[32], path[1024];
    if (i < 6) wcscpy(name, names[i]); else swprintf(name, 32, L"%ls%d.ext", i < 16 ? L"Com" : L"lPT", (i - 6) % 10);
    for (int ancestor = 0; ancestor < 2; ++ancestor) {
      swprintf(path, 1024, L"%ls\\%ls%ls", root, name, ancestor ? L"\\token" : L"");
      Job j = {0}; j.path = path; opens = 0;
      REQUIRE(!pin_ancestors(&j) && j.error == ERROR_INVALID_NAME);
      REQUIRE(opens == 0);
    }
  }
  REQUIRE(!reserved_component(L"console.txt", 11));
  REQUIRE(!reserved_component(L"COM10", 5));
}
static void bounded_aces(void) {
  BYTE storage[256] = {0}; PACL acl = (PACL)storage;
  acl->AclSize = sizeof(storage); acl->AceCount = 1;
  fake_ace = (ACE_HEADER *)(storage + sizeof(ACL));
  fake_ace->AceType = ACCESS_ALLOWED_ACE_TYPE;
  SID *sid = (SID *)((BYTE *)fake_ace + offsetof(ACCESS_ALLOWED_ACE, SidStart));
  sid->Revision = SID_REVISION; sid->SubAuthorityCount = 1;
  malformed = 1;
  WORD sizes[] = { 0, sizeof(ACE_HEADER), 8, 15, 255 };
  for (size_t i = 0; i < sizeof(sizes) / sizeof(sizes[0]); ++i) {
    fake_ace->AceSize = sizes[i]; sid_calls = 0;
    REQUIRE(ordinary_ace(acl, 0) == NULL && sid_calls == 0);
  }
  fake_ace->AceSize = 20; sid->SubAuthorityCount = 15; sid_calls = 0;
  REQUIRE(ordinary_ace(acl, 0) == NULL && sid_calls == 0);
  sid->SubAuthorityCount = 1;
  REQUIRE(ordinary_ace(acl, 0) != NULL);
  sid->Revision = 0; sid_calls = 0;
  REQUIRE(ordinary_ace(acl, 0) == NULL && sid_calls == 1);
  sid->Revision = SID_REVISION;
  fake_ace = (ACE_HEADER *)(storage + 255); sid_calls = 0;
  REQUIRE(ordinary_ace(acl, 0) == NULL && sid_calls == 0);
  malformed = 0;
}
static void secure_handle(const wchar_t *root) {
  wchar_t parent[1024], child[1024];
  swprintf(parent, 1024, L"%ls\\repair", root); swprintf(child, 1024, L"%ls\\child", parent);
  Job user = {0}; REQUIRE(user_sid(&user));
  /* Create with an explicit user owner so this fixture changes only the DACL.
   * The setup handle can query metadata and enumerate children for SetSecurityInfo;
   * the actual repair below still uses the addon's exact READ_CONTROL/WRITE_DAC handle. */
  run_create(parent, 1);
  HANDLE h = CreateFileW(parent, GENERIC_READ | WRITE_DAC, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, NULL);
  REQUIRE(h != INVALID_HANDLE_VALUE);
  BYTE acl_storage[512] = {0}, users[SECURITY_MAX_SID_SIZE]; DWORD sid_size = sizeof(users);
  REQUIRE(CreateWellKnownSid(WinBuiltinUsersSid, NULL, users, &sid_size));
  PACL acl = (PACL)acl_storage;
  REQUIRE(InitializeAcl(acl, sizeof(acl_storage), ACL_REVISION));
  REQUIRE(AddAccessAllowedAceEx(acl, ACL_REVISION, 3, FILE_ALL_ACCESS, user.sid));
  REQUIRE(AddAccessAllowedAceEx(acl, ACL_REVISION, 3, FILE_GENERIC_READ, users));
  DWORD error = SetSecurityInfo(h, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
      NULL, NULL, acl, NULL);
  if (error != ERROR_SUCCESS) fprintf(stderr, "Fixture SetSecurityInfo failed: %lu\n", error);
  REQUIRE(error == ERROR_SUCCESS);
  PSID owner = NULL; PACL installed = NULL; PSECURITY_DESCRIPTOR setup = NULL;
  error = GetSecurityInfo(h, SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
      &owner, NULL, &installed, NULL, &setup);
  REQUIRE(error == ERROR_SUCCESS);
  REQUIRE(owner && EqualSid(owner, user.sid));
  REQUIRE(installed && installed->AceCount == 2);
  LocalFree(setup); CloseHandle(h);
  HANDLE file = CreateFileW(child, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, CREATE_NEW, 0, NULL); REQUIRE(file != INVALID_HANDLE_VALUE); CloseHandle(file);
  PSECURITY_DESCRIPTOR before = NULL, after = NULL; PACL before_acl = NULL, after_acl = NULL;
  REQUIRE(GetNamedSecurityInfoW(child, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, NULL, NULL, &before_acl, NULL, &before) == ERROR_SUCCESS);
  REQUIRE(before_acl && before_acl->AceCount == 2);
  Job *j = job(parent, 3, 1); execute(NULL, j);
  if (j->reason) fprintf(stderr, "secure directory failed: %s (Win32 %lu)\n", j->reason, j->error);
  REQUIRE(!j->reason); release_job(j);
  REQUIRE(GetNamedSecurityInfoW(child, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, NULL, NULL, &after_acl, NULL, &after) == ERROR_SUCCESS);
  REQUIRE(before_acl && after_acl && before_acl->AclSize == after_acl->AclSize &&
      !memcmp(before_acl, after_acl, before_acl->AclSize));
  REQUIRE(before && after);
  if (before && after) {
    DWORD n = GetSecurityDescriptorLength(before);
    REQUIRE(n == GetSecurityDescriptorLength(after) && !memcmp(before, after, n));
  }
  LocalFree(before); LocalFree(after); free(user.token); free(user.owner_token);
  DeleteFileW(child); RemoveDirectoryW(parent);
}
int main(int argc, char **argv) {
  if (argc != 2) { fprintf(stderr, "Expected one test name\n"); return 2; }
  wchar_t profile[768], root[1024];
  REQUIRE(GetEnvironmentVariableW(L"USERPROFILE", profile, 768));
  swprintf(root, 1024, L"%ls\\private-files-native-%lu", profile, GetCurrentProcessId());
  run_create(root, 1);
  if (!strcmp(argv[1], "directory-identity")) directory_identity(root);
  else if (!strcmp(argv[1], "private-write")) private_write(root);
  else if (!strcmp(argv[1], "owner-policy")) owner_policy();
  else if (!strcmp(argv[1], "safe-parent-aces")) safe_parent_aces();
  else if (!strcmp(argv[1], "napi-failures")) napi_failures();
  else if (!strcmp(argv[1], "zeroize")) zeroize(root);
  else if (!strcmp(argv[1], "token-queries")) token_queries();
  else if (!strcmp(argv[1], "reserved-names")) reserved_names(root);
  else if (!strcmp(argv[1], "bounded-aces")) bounded_aces();
  else if (!strcmp(argv[1], "secure-handle")) secure_handle(root);
  else if (!strcmp(argv[1], "delay-hook")) {
    DelayLoadInfo info = {0}; info.szDll = "node.exe";
    REQUIRE(host_napi_import(dliNotePreLoadLibrary, &info) == (FARPROC)GetModuleHandleW(NULL));
    REQUIRE(host_napi_import(dliFailLoadLib, &info) == NULL);
    info.szDll = "other.dll";
    REQUIRE(host_napi_import(dliNotePreLoadLibrary, &info) == NULL);
  }
  else { fprintf(stderr, "Unknown test: %s\n", argv[1]); ++failures; }
  RemoveDirectoryW(root);
  printf("%s: %s (%d failures)\n", argv[1], failures ? "FAIL" : "PASS", failures);
  return failures ? 1 : 0;
}
