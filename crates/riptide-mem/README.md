# riptide-mem

The title's memory: banks of RAM the GPU uses in place, the frames they are made of, the title's addresses and what is mapped at them, write guards and the RAM budget.

The design is [`docs/design/2026-11-27-title-memory.md`](../../docs/design/2026-11-27-title-memory.md), decisions M1 to M8.

## Interface

**Banks and frames.** `TitleMemory::new(max_bytes)` makes title memory that may grow to that size. Memory comes in banks of 2 GiB (`BANK_BYTES`), each a committed section of RAM, made when first needed. A bank is cut into frames of 64 KiB (`FRAME`), numbered across banks. A bank's size is charged against the page file when it is made; only pages that are touched use RAM.

**Addresses.** A title's addresses are host addresses: its code runs natively and uses them as pointers.

- `reserve(bytes, limits)` reserves addresses and maps nothing. `Limits` gives the lowest and highest address and an alignment.
- `commit(address, bytes, limits, access)` takes frames and maps them: at `address`, inside a reservation, or with `None` in a new reservation. The memory reads as zero.
- `unmap(address, bytes)` takes away what is mapped in a range. `release(address)` gives a whole reservation back.

**Physical pages.** The console lets a title hold pages and map them itself.

- `allocate_frames(count)` hands out frames by number, contiguous in one bank when any bank has room.
- `map_frames(address, frames, access)` maps them one after another, in place of what was there. A frame can be mapped at several addresses at once; a write through one shows at the others.
- `free_frames(frames)` takes them back. One that is still mapped stays readable there and goes back when its last mapping does.

**The page table.** `locate(address, bytes)` says where in the banks a range of title addresses lies: a list of parts, each a bank, an offset in it and a length. One part means the GPU can use the range where it is; several mean it has to be copied. `None` means some of it is not mapped.

**The GPU.** `open_heap(bank, device, flags)` opens a bank as a D3D12 heap, from its address, with one buffer over all of it. Offset `n` of the buffer is byte `n` of the bank. The GPU reads and writes every byte of the bank, mapped or not: page protection applies to the CPU only. Every heap must be dropped before the `TitleMemory` it lies on.

**Write guards.** `guard(address, bytes, resource)` write-protects the pages of a resource, at every address their frames are mapped at (decisions D3, D4 and M7). The first write to any of them, through any of those addresses, takes the protection off all of them, puts `resource` in the list `take_written()` returns, and then goes on as if nothing had happened; later writes cost nothing. `unguard(resource)` ends a guard without a report. Reads are never disturbed.

- Pages are protected whole. A write beside a resource but on one of its pages ends its guard too: it is reported as written though it was not, which costs a copy and never a wrong picture. Two resources that share a page are both reported; a page shared with a guard that is still live stays protected.
- Unmapping guarded pages ends their guards and reports them.
- Guard a resource before copying it, not after: a write between the copy and the guard is never heard of.
- The fault is caught by a vectored exception handler. It answers only for write faults on guarded pages and passes every other fault on, so a title's exception handling keeps working.
- When several threads write one guarded resource at once, the first ends the guard and the others find the page writable and try again. One report, no lost write.
- A system call that writes into a guarded page fails instead of faulting, and a GPU write passes a guard without a fault. Whoever makes such a write for the title has to end the guard first.

**Freed frames give their RAM back** (decisions D10 and M8). A frame that is no longer mapped anywhere and that the title does not hold is discarded through its bank. A frame that has been used is zeroed before it is handed out again, because Windows does not say what a discarded page holds. `discard_failures()` counts the times Windows refused.

**The RAM budget** (decision D10).

- `RamBudget::from_settings(&settings)` sets the limit once, at launch: the `ram_budget_mib` setting if there is one, otherwise the RAM that is free, and never more than 16 GiB. `RamBudget::new(setting, free_ram)` is the same rule without reading the machine.
- `memory.ram_use()` is what the process uses now: `title`, the frames handed out, and `private`, everything else the process has allocated. `committed_bytes()` is always current.
- `budget.pressure(use)` says `Under`, `Near` (90% or more) or `Over`, and `budget.headroom(use)` what is left.
- `memory.sample(video_bytes)` gives the figures `riptide-trace` asks for at the end of each frame.

Title memory is shared sections, so Windows does not count it in the process's "private bytes"; it shows in the working set. `RamUse` adds the two.

## Depends on

- `riptide-config`
- `riptide-trace`

## Status

Piece 3 of the build order, rebuilt on 2026-11-27 for decisions M1 to M8. `riptide-hle`'s console memory calls run on it.

Working and tested without a GPU (`cargo test -p riptide-mem`): frames, reservations, commit, physical pages at one and at several addresses, the page table, write guards through any address, and frames going back.

Not yet run: the GPU test with the new banks (`cargo test -p riptide-mem --test gpu -- --ignored`). The way it opens a bank was measured on the target machine with a one-off test (see `docs/measurements.md`): it works, the GPU sees writes made through a second mapping, and nothing stays committed after the process exits.

Not built: nothing acts on the budget yet, and 2 MiB large pages (Q12) are not used.
