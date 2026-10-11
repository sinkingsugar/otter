/* Per-block instruction counts under QEMU's user-mode emulator: for each
 * translated block, its start address and the instructions it executed
 * (runs times length), written to the file named by `out` at exit. A
 * profile for another architecture, as Callgrind's is for the host one
 * (bench/instructions/README.md); `profile.py` attributes it to functions.
 *
 * Build: cc -shared -fPIC -O2 $(pkg-config --cflags glib-2.0) -o libinsnblocks.so insnblocks.c
 */
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <qemu-plugin.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

struct block {
  uint64_t pc;
  uint64_t insns;
  uint64_t runs;
  struct block *next;
};

static struct block *blocks;
static char out[4096];

static void on_exec(unsigned int vcpu, void *userdata) {
  ((struct block *)userdata)->runs++;
}

static void on_translate(struct qemu_plugin_tb *tb, void *userdata) {
  struct block *b = calloc(1, sizeof *b);
  b->pc = qemu_plugin_tb_vaddr(tb);
  b->insns = qemu_plugin_tb_n_insns(tb);
  b->next = blocks;
  blocks = b;
  qemu_plugin_register_vcpu_tb_exec_cb(tb, on_exec, QEMU_PLUGIN_CB_NO_REGS, b);
}

static void at_exit(void *userdata) {
  FILE *f = fopen(out, "w");
  if (!f) {
    return;
  }
  /* Where the main binary's text landed, to place its symbols. */
  fprintf(f, "text %" PRIx64 "\n", qemu_plugin_start_code());
  for (struct block *b = blocks; b; b = b->next) {
    if (b->runs) {
      fprintf(f, "%" PRIx64 " %" PRIu64 "\n", b->pc, b->insns * b->runs);
    }
  }
  fclose(f);
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id, const qemu_info_t *info,
                                           int argc, char **argv) {
  for (int i = 0; i < argc; i++) {
    if (strncmp(argv[i], "out=", 4) == 0) {
      snprintf(out, sizeof out, "%s", argv[i] + 4);
    }
  }
  if (!out[0]) {
    fprintf(stderr, "insnblocks: needs out=<file>\n");
    return -1;
  }
  qemu_plugin_register_vcpu_tb_trans_cb(id, on_translate, NULL);
  qemu_plugin_register_atexit_cb(id, at_exit, NULL);
  return 0;
}
