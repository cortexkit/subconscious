#include <stdio.h>
#include "../native/open-policy.h"

static int check(const char *name, int valid) {
  printf("%s %s\n", valid ? "PASS" : "FAIL", name);
  return !valid;
}

int main(void) {
  int failures = 0;
  PrivateOpenPolicy classify = private_open_policy(2);
  PrivateOpenPolicy fallback = private_classify_fallback();
  PrivateOpenPolicy repair = private_open_policy(3);
  const uint32_t data_rights = 0xc0000007u;
  failures += check("classification_open_has_no_data_rights",
      !(classify.access & data_rights) && classify.access == 0x00120080u);
  failures += check("owner_implicit_classification_fallback_is_exact_read_control",
      fallback.access == 0x00020000u && fallback.exact);
  failures += check("repair_open_uses_only_exact_owner_implicit_rights",
      repair.access == 0x00060000u && repair.exact);
  failures += check("read_open_requests_data_on_the_verified_io_handle",
      private_open_policy(1).access == 0x80020000u && !private_open_policy(1).exact);
  failures += check("standard_user_security_paths_never_request_sacl",
      PF_QUERY_SECURITY_INFO == 0x00000005u && PF_SET_PRIVATE_DACL == 0x80000004u &&
      !((PF_QUERY_SECURITY_INFO | PF_SET_PRIVATE_DACL) & 0x00000008u) &&
      !((classify.access | fallback.access | repair.access | private_open_policy(0).access |
          private_open_policy(1).access) & 0x01000000u));
  printf("5 open/security policy checks; %d failures\n", failures);
  return failures ? 1 : 0;
}
