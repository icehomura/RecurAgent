/* Compile-only ABI declaration check; no provider/runtime calls. */
#include "ra.h"

void ra_header_contract(void) {
    char *(*run)(RaRuntime *, const char *) = ra_run_task;
    char *(*take_partial)(void) = ra_take_last_partial_result;
    const char *(*diagnostic)(void) = ra_last_error;
    void (*release)(char *) = ra_string_free;
    char *(*memory_upsert)(RaRuntime *, const char *) = ra_memory_upsert;
    char *(*memory_search)(RaRuntime *, const char *) = ra_memory_search;
    char *(*memory_load)(RaRuntime *, const char *) = ra_memory_load;
    char *(*memory_stats)(RaRuntime *) = ra_memory_stats;
    char *(*model_status)(const char *) = ra_embedding_model_status;
    char *(*model_ensure)(const char *, bool) = ra_embedding_model_ensure;
    (void)run;
    (void)take_partial;
    (void)diagnostic;
    (void)release;
    (void)memory_upsert;
    (void)memory_search;
    (void)memory_load;
    (void)memory_stats;
    (void)model_status;
    (void)model_ensure;
}
