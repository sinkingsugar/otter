/* Counts the guest instructions a program executes under QEMU's user-mode
 * emulator, exactly, and writes the total to the file named by the plugin
 * argument `out` at exit. The instruction gate's local stand-in for
 * Cachegrind on another architecture (bench/instructions/README.md):
 * deterministic, but counting the target's own libc, so totals differ from
 * Cachegrind's; differences between two revisions are what it is for.
 *
 * Build: cc -shared -fPIC -O2 $(pkg-config --cflags glib-2.0) -o libinsncount.so insncount.c
 * (QEMU 11's plugin API; run.py builds it)
 * Use:   qemu-aarch64 -L /usr/aarch64-linux-gnu \
 *          -plugin ./libinsncount.so,out=count.txt ./program args...
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <qemu-plugin.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

static struct qemu_plugin_scoreboard *counts;
static qemu_plugin_u64 count;
static char out[4096];

static void on_translate(struct qemu_plugin_tb *tb, void *userdata) {
  /* Each time the block runs, add its instruction count, inline. */
  qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
      tb, QEMU_PLUGIN_INLINE_ADD_U64, count, qemu_plugin_tb_n_insns(tb));
}

static void at_exit(void *userdata) {
  uint64_t total = qemu_plugin_u64_sum(count);
  FILE *f = fopen(out, "w");
  if (f) {
    fprintf(f, "%" PRIu64 "\n", total);
    fclose(f);
  }
  qemu_plugin_scoreboard_free(counts);
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id, const qemu_info_t *info,
                                           int argc, char **argv) {
  for (int i = 0; i < argc; i++) {
    if (strncmp(argv[i], "out=", 4) == 0) {
      snprintf(out, sizeof out, "%s", argv[i] + 4);
    }
  }
  if (!out[0]) {
    fprintf(stderr, "insncount: needs out=<file>\n");
    return -1;
  }
  counts = qemu_plugin_scoreboard_new(sizeof(uint64_t));
  count = qemu_plugin_scoreboard_u64(counts);
  qemu_plugin_register_vcpu_tb_trans_cb(id, on_translate, NULL);
  qemu_plugin_register_atexit_cb(id, at_exit, NULL);
  return 0;
}
