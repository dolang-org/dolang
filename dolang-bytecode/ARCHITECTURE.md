# Bytecode and Verification Architecture

The `dolang-bytecode` crate defines the Do bytecode format, instruction set,
and verification system.

## Bytecode Format

Files consist of a header (`\xffdobytec` + version) followed by serialized
tables: string table, symbol table, constant table, pack/unpack tables (for
call and function signatures and object destructuring), function table with
validation certificates, and debug information.

## Instruction Set

Do uses a stack-based instruction set, but the operand stack depth is
statically known at all instructions, an attribute shared with the JVM and .NET
CLR. This makes the operand stack more of a compressed instruction encoding
scheme.

## Verification System

Bytecode verification ensures safety through multiple passes: syntax validation
checks instruction well-formedness and bounds; basic block identification
determines control flow structure; dataflow analysis computes abstract VM
states across all instruction paths, producing a certificate; alternatively, an
existing certificate can be verified to be correct (a dataflow fixed point of
the function bytecode).

## Certificates

Each function includes a certificate with maximum operand depth and
pre-computed block states. During loading, certificates are verified by
performing a single pass over all instructions, which is cheaper than computing
them from scratch in the presence of complex data flow. Note that this is of
marginal benefit at the moment since the abstract VM state is relatively simple
and converges rapidly, but future extensions which perform more complex
analysis or that cope with more complex branching (e.g. exception unwind
tables) will benefit from pre-computation.

## Safety Properties

Verified functions should not reach malformed instructions, "fall off" the end
of the bytecode, exceed the bounds of the operand stack or otherwise access any
out-of-bounds index. The overall bytecode file is also extensively checked to
ensure it's syntactically valid, has no out-of-bounds references, and conforms
to certain static limits in terms of resource consumption.
