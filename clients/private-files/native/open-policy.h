#ifndef PRIVATE_FILES_OPEN_POLICY_H
#define PRIVATE_FILES_OPEN_POLICY_H
#include <stdint.h>

/* Numeric Windows access masks are kept portable so tests exercise the actual
 * policy used by the addon without pretending to exercise a Windows filesystem. */
#define PF_READ_CONTROL 0x00020000u
#define PF_WRITE_DAC 0x00040000u
#define PF_READ_ATTRIBUTES 0x00000080u
#define PF_SYNCHRONIZE 0x00100000u
#define PF_GENERIC_READ 0x80000000u
#define PF_GENERIC_WRITE 0x40000000u
#define PF_DELETE 0x00010000u
#define PF_QUERY_SECURITY_INFO 0x00000005u
#define PF_SET_PRIVATE_DACL 0x80000004u

typedef struct { uint32_t access; int exact; } PrivateOpenPolicy;

static inline PrivateOpenPolicy private_open_policy(int operation) {
  PrivateOpenPolicy policy = { PF_READ_CONTROL, 0 };
  if (operation == 0) policy.access |= PF_GENERIC_WRITE | PF_DELETE;
  if (operation == 1) policy.access |= PF_GENERIC_READ;
  if (operation == 2) policy.access |= PF_READ_ATTRIBUTES | PF_SYNCHRONIZE;
  if (operation == 3) { policy.access |= PF_WRITE_DAC; policy.exact = 1; }
  return policy;
}

static inline PrivateOpenPolicy private_classify_fallback(void) {
  PrivateOpenPolicy policy = { PF_READ_CONTROL, 1 };
  return policy;
}
#endif
