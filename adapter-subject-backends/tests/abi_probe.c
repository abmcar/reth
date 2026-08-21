#include <stddef.h>
#include <stdio.h>

#include "evmc/evmc.h"

#define PRINT_SIZE(type) printf("size." #type "=%zu\n", sizeof(type))
#define PRINT_OFFSET(type, field) \
  printf("offset." #type "." #field "=%zu\n", offsetof(type, field))

int main(void) {
  PRINT_SIZE(evmc_address);
  PRINT_SIZE(evmc_bytes32);
  PRINT_SIZE(struct evmc_message);
  PRINT_SIZE(struct evmc_tx_context);
  PRINT_SIZE(struct evmc_tx_initcode);
  PRINT_SIZE(struct evmc_result);
  PRINT_SIZE(struct evmc_host_interface);
  PRINT_SIZE(struct evmc_vm);

  PRINT_OFFSET(struct evmc_message, gas);
  PRINT_OFFSET(struct evmc_message, input_data);
  PRINT_OFFSET(struct evmc_message, value);
  PRINT_OFFSET(struct evmc_message, code_address);
  PRINT_OFFSET(struct evmc_message, code);
  PRINT_OFFSET(struct evmc_message, code_size);

  PRINT_OFFSET(struct evmc_tx_context, blob_hashes);
  PRINT_OFFSET(struct evmc_tx_context, initcodes);
  PRINT_OFFSET(struct evmc_result, gas_left);
  PRINT_OFFSET(struct evmc_result, gas_refund);
  PRINT_OFFSET(struct evmc_result, release);
  PRINT_OFFSET(struct evmc_result, create_address);

  PRINT_OFFSET(struct evmc_host_interface, set_transient_storage);
  PRINT_OFFSET(struct evmc_vm, execute);
  PRINT_OFFSET(struct evmc_vm, set_option);
  return 0;
}
