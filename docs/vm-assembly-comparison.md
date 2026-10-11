# Release VM assembly comparison

Inspected 2026-10-05, using the actual binaries from the
[compose-cleanup benchmark](vm-execution-benchmarks.md#compose-time-scratch-release-selection).
1.x is the existing GCC release build at `37597927e`; 2.0 is Rust 1.98.1 default
release, based on `4cbfb0e` with compose-time cleanup selection. Binary hashes,
exact objdump commands and raw excerpts are in
[the result directory](../bench/vm-execution/results/2026-10-05-compose-cleanup/).
No debug build, tracing instrumentation, or toy replacement loop was used.

The comparison is `shards::activateShards` in 1.x versus
`otter_core::inline::run` in 2.0. Both contain the hot inline handlers; comparing
only out-of-line shard methods would miss these paths. 1.x handler addresses
were resolved using its generated `InlineShard` enum and the relative jump
table at `0x936da8c`. The excerpts below omit surrounding dispatch and error
paths where noted. Registers are those in the measured binaries.

## Const: both forward a pointer

1.x (`0x1b6abd1`, handler selected by `CoreConst`):

```asm
lea r12, [rbx+0xd0]       ; address of the shard's stored SHVar
```

2.0 no-release Const (`0x24a327`, then common tail `0x24a68e`):

```asm
add r12, 0x8             ; address of the instruction's stored Var
jmp const_common_tail
mov r14, r12             ; accumulator pointer
jmp next_instruction
```

Neither copies/refcounts the value per Const. Necessary ownership release is
in a separate compose-selected opcode, not a check in this fast handler.
The remaining Const gap therefore involves dispatch/loop/boundary costs, not
an expensive constant operation or an iterator abstraction.

## Get: pre-resolved pointer versus frame-slot resolution

1.x (`0x1b6ab3e`, `CoreGet`):

```asm
mov r12, [rbx+0xd8]       ; cached variable-cell pointer
```

This corresponds to `GetRuntime::core._cell`, resolved before activation.

2.0 no-release Get (`0x24a4ad`):

```asm
movzx ecx, byte ptr [r12+0x8] ; Local or Mesh binding
mov   rax, [r12+0x10]        ; slot index
 test cl, cl
mov   rdx, [rsp+0x1b0]       ; local frame length
cmovne rdx, [rsp+0x1c0]      ; or mesh frame length
cmp   rax, rdx
jae   bounds_failure
 test cl, cl
mov   rcx, [rsp+0x58]        ; local frame base
cmovne rcx, [rsp+0x1b8]      ; or mesh frame base
lea   rax, [rax+rax*2]
lea   r14, [rcx+rax*8]       ; base + index * 24 (sizeof Var)
jmp   next_instruction
```

The local/mesh selection uses conditional moves here, not conditional jumps,
but it still consumes instructions and dependencies. The bounds check has a
conditional branch. No scratch check, Var clone or Arc increment occurs in
this fast Get handler. Compose already knows the binding kind/index: separate
Local/Mesh opcodes and precomputed byte offsets are concrete future experiments.
Eliminating bounds checks would additionally require a proved frame-layout
contract at entry; it should not be done by simply deleting the check.

1.x's cached pointer lives in per-instance shard state. Our compiled stream is
shared by instances with different frame addresses. Storing instance pointers
in that shared stream would violate the design. Specialized offsets, or an
explicit per-instance binding table, can preserve the split.

## Dispatch: the Rust Op representation adds decoding

1.x (`0x1b6aae8`) reads a plain integer opcode and indexes a jump table:

```asm
mov    ecx, [rbx]
cmp    ecx, 0x2b
ja     generic_handler
mov    esi, ecx
movsxd rdi, dword ptr [r14+rsi*4]
add    rdi, r14
jmp    rdi
```

2.0 (`0x24a2e4`) calculates the 32-byte instruction address, then decodes the
compiler-selected enum representation before its jump table:

```asm
mov    rax, rbp
shl    rax, 5
mov    rcx, [rsp+0x170]
lea    r12, [rcx+rax]
mov    rax, [rcx+rax]
mov    rcx, rax
movabs rdx, 0x8000000000000000
xor    rcx, rdx
 test  rax, rax
mov    eax, 0xc
cmovns rcx, rax
lea    rdx, [jump_table]
movsxd rax, dword ptr [rdx+rcx*4]
add    rax, rdx
jmp    rax
```

That is real generated overhead. The high-bit decode is consistent with Rust
using an otherwise invalid payload representation to encode enum variants;
the important observable fact is that dispatch is not a plain byte-tag load.
This is the **Op layout**, distinct from the public **Var layout**.
An explicit opcode representation is worth benchmarking; fewer decode
instructions can trade against instruction size/alignment and cache footprint.
This inspection does not claim that changing `repr` alone achieves parity.

The complete loops have other differences: 1.x checks context flow state after
each activation and advances a null-terminated array of shard pointers. 2.0
increments an index and checks instruction count; its uninterrupted segment has
no per-op context flow-state check. Rust reloads some frame/code metadata from
stack slots. Both use indirect dispatch. Counting only one side's handler or
only the number of assembly lines would be misleading.

## Add: packed arithmetic is already present

1.x's `Math.Add` compose chooses Int64x2 for **Int or Int2**, Float64x2 for
**Float or Float2**, and Float32x4 for **Float3 or Float4** (source:
`shards/modules/core/math_binary.hpp`). Scalar values use the relevant low lane
of the same aligned 16-byte payload operations:

```asm
; Int path, 0x1b6b190
mov     rsi, [rbx+0x110]       ; resolved RHS pointer
vmovdqa xmm8, [rsi]
vpaddq  xmm9, xmm8, [r12]
lea     r12, [rbx+0xd0]
vmovdqa [rbx+0xd0], xmm9
; Float path uses vaddpd; Float4 uses vaddps
```

The handlers also read the context flow state, then join the common loop tail.
They do not check input tags, rebuild the output tag, or branch on integer
overflow. The packed integer instruction wraps lane overflow. 2.0 deliberately
reports integer overflow and retains a runtime input-tag check:

```asm
; AddIntConst, 0x24a477
cmp  byte ptr [r14], 2
jne  invalid_input
mov  rax, [r14+8]
add  rax, [r12+8]
jo   overflow_error
mov  byte ptr [rsp+0xb0], 2
mov  [rsp+0xb8], rax
lea  r14, [rsp+0xb0]
jmp  next_instruction
```

For Float4, Rust uses `movups` + **`addps`** + `movups`; 1.x uses aligned AVX
encodings with **`vaddps`**. Both perform four packed float additions. This
build difference also reflects target flags: 1.x uses `-march=broadwell`, while
2.0 uses default Cargo release targeting. No runtime arithmetic performance
claim should be attributed solely to the mnemonic prefix or alignment.

1.x writes each result into the current shard's `_result`; Rust reuses one
numeric scratch slot across the segment. Rust also embeds constant operands
in instructions, whereas the inspected 1.x handler loads a resolved operand
pointer. These differences help explain why extra checks do not automatically
make Rust slower: the matched scalar Int chain is actually faster in 2.0.

## What the evidence supports

At width 256 the paired median 2.0 stackless / 1.x time ratios are Const(Int)
1.44×, Get(Int) 1.99×, Add(Int) 0.75×, Add(Float) 0.98× and Add(Float4) 1.03×.
The clearest remaining cheap-shard candidates are opcode decoding and binding
resolution. This is static assembly inspection plus measured end-to-end chains,
not a hardware-counter attribution of the exact time spent in each instruction.

Next experiments: explicit opcode representation; Local/Mesh-specialized
bindings with precomputed offsets; then fusion that retains source/error and
ownership boundaries. Arithmetic already has packed SIMD where relevant.
Removing overflow semantics or trusting arbitrary host output without checking
would be a separate contract decision, not a free compiler optimization.

## Implemented follow-up: explicit tags and specialized Get offsets

The next implementation replaces the default `Op` encoding with `#[repr(u8)]`
and selects Local/Mesh Get opcodes containing a checked, compose-computed byte
offset. Release/non-release Get variants retain the preceding ownership fix.
The table-constructor variant places its small Type field before its Vec so the
explicit tag does not inflate the measured x86-64 instruction stride: it stays
32 bytes. Public Var remains 24 bytes; there are no new per-instance fields.

A four-way comparison separates the changes: baseline, explicit tag only,
specialized offsets only, and both. Each cell uses the same script and iteration
count with sequential, rotating/reversing binary order on CPU 2. There are three
process rounds, each with two warmups and three retained batches. Both schedulers
are measured at widths 8 and 256, alongside 1.x; every output and count is checked.
Nine workload families include Const/Get, assignment, arithmetic and sequence
Take. Frequency is unlocked, so these are empirical results on this machine,
not guarantees of a specific percentage on another CPU.

Width-256 stackless medians, ns per chain:

| Workload | Before | Explicit tag | Offsets only | Both | 1.x |
|---|---:|---:|---:|---:|---:|
| Const(Int) | 180.3 | 141.6 | 173.3 | 127.3 | 124.8 |
| Const(Seq) | 184.2 | 142.8 | 177.7 | 129.8 | 194.3 |
| Get(Int) | 254.9 | 210.1 | 194.3 | 145.5 | 130.7 |
| Get(Seq) | 256.4 | 212.4 | 196.1 | 149.3 | 205.6 |
| Assign(Int) | 680.1 | 599.4 | 591.4 | 555.9 | 449.8 |
| Add(Int) | 223.2 | 177.7 | 228.2 | 178.5 | 295.7 |
| Add(Float) | 365.6 | 358.5 | 363.8 | 361.9 | 372.2 |
| Add(Float4) | 380.9 | 372.8 | 379.3 | 374.7 | 370.4 |
| Take(Seq) | 759.2 | 668.6 | 672.9 | 626.4 | 1,586.3 |

Changing opcode variants also changes whole-function code generation: even
Const changes with Get specialization. These effects are not independently
additive, and the matrix is more informative than subtracting assembly counts.

The combined candidate's dispatch loads a byte tag directly before the jump
table. The high-bit decode is gone. Local Get is now:

```asm
mov r14, [r13+8]          ; precomputed byte offset
cmp r14, [rsp+0x128]      ; local frame byte length, computed once at entry
jae bounds_failure
add r14, rbp             ; local frame base
jmp next_instruction
```

Mesh Get has its own handler. There is no per-Get frame-kind selection or
index multiplication. Bounds checks remain: SlotOffset construction checks
multiplication overflow and guarantees whole-Var alignment; offset < frame byte
length therefore proves the entire Var fits. Raw bases still derive from the
original exclusive frame borrows. The analysis/safety model does not rely on
unchecked offsets or instance pointers stored in shared compiled data.

The measured inline function grows from 7,553 to 7,642 bytes; its explicit stack
reservation grows from 376 to 392 bytes on this x86-64 build. Instruction storage
and persistent per-instance state do not grow. This is a small code/stack cost
for the measured throughput gain, not a claim that representation is free.

Empirical conclusion: the existing ownership model and compiled/state split
support these optimizations, while the compiler-default instruction encoding
was demonstrably suboptimal for these workloads. Correctness evidence includes
the shared acceptance suite, retained lifetime/alias tests, new wrong-frame
bounds checks, WASI execution, and nine inline Miri tests. These checks cover the
implementation's contracts; they do not prove an optimal representation for all
future shards or workloads. See the [full-suite follow-up](vm-execution-benchmarks.md#explicit-opcode-tags-and-specialized-get-addressing)
for broader performance coverage, raw matrix samples, reproducible variant
patches and final-binary assembly.
