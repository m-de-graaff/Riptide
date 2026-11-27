# riptide-mem

The title's memory: one committed block of RAM that is also a D3D12 heap, its allocator, write guards, and console-to-host address conversion.

## Interface

**The block.** `TitleMemory::create(bytes)` makes one committed section of RAM and maps it once. A title's addresses are host addresses inside that view: the title's code runs natively and uses them as pointers. The whole block is charged against the page file at once; only pages that are touched use RAM.

**The allocator.** The title's memory calls go through these, which behave as the console's (and Windows') virtual memory calls do:

- `reserve(address, size)` reserves addresses, at `address` or at the lowest free place. A reservation starts on a 64 KiB boundary (`GRANULARITY`) and covers whole 4 KiB pages (`PAGE`).
- `commit(address, size)` commits the pages covering a range inside one reservation. New pages read as zero; pages already committed keep their bytes.
- `decommit(address, size)` makes pages inaccessible again; a size of zero means the whole reservation. They read as zero when committed again.
- `release(address)` frees the reservation that starts at `address`.
- `allocate(size)` reserves and commits in one call. `query(address)` reports the run of pages around an address that share one state: free, reserved or committed.

Pages that are not committed are inaccessible to the CPU, so a stray access faults as it would on the console. Failures are a `MemError`, which `riptide-hle` turns into the console's error codes. All calls can be made from any thread.

**Address conversion.** `address_map()` gives an `AddressMap { base, size }`. `map.offset(address)` is the offset in the buffer: one subtraction. `checked_offset` refuses an address outside title memory, and `address(offset)` goes the other way.

**The heap.** `open_heap(device, flags)` opens the block as a D3D12 heap with one buffer over all of it. Offset `n` of the buffer is the byte at title address `base + n`. The GPU reads and writes every page, committed or not: page protection applies to the CPU only.

**Write guards.** `guard(address, size, resource)` write-protects the pages of a resource (decisions D3 and D4). The first write to any of them takes the protection off the whole resource, puts `resource` in the list `take_written()` returns, and then goes on as if nothing had happened; later writes cost nothing. `unguard(resource)` ends a guard without a report. Reads are never disturbed.

- Pages are protected whole. A write beside a resource but on one of its pages ends its guard too: it is reported as written though it was not, which costs a copy and never a wrong picture. Two resources that share a page are both reported; a page shared with a guard that is still live stays protected.
- Decommitting or releasing guarded pages ends their guards and reports them.
- The fault is caught by a vectored exception handler, which Windows calls before the title's own handlers. It answers only for write faults on guarded pages and passes every other fault on, so a title's exception handling keeps working.
- When several threads write one guarded resource at once, the first ends the guard and the others find the page writable and try again. One report, no lost write.

## Depends on

- `riptide-trace` (not used yet)

## Status

Piece 3 of the build order. Working: the block, the allocator, address conversion, the heap and write guards. The GPU test (`cargo test -p riptide-mem --test gpu -- --ignored`) needs a GPU and has passed on a development laptop with a 64 MiB block; it has not run on the target machine, and the 14 GiB block waits for the probe's answer to open question Q1.

Not built yet: the RAM budget, and returning freed pages to Windows. Until the last one, decommitted pages keep their RAM, and are zeroed when committed again.
