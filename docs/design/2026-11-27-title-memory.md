# Title memory: banks, frames and a page table

Decided 2026-11-27, after the probe answered Q1 and the real title showed how it asks for memory. It replaces the form of D1 and D2 in `2026-11-27-architecture.md`; their aim stands: the GPU uses the title's bytes where they are, with no mirror.

## Why the first form does not hold

The first form was one block of RAM, opened as one D3D12 heap with one buffer, with a console address becoming a buffer offset by one subtraction. Three facts from the target machine rule it out. The numbers are in `docs/measurements.md`.

- **A heap must be smaller than 4 GiB.** Sections of 4 GiB and more are refused, whether the heap is opened from the file mapping or from an address.
- **A heap opened from the file mapping stays committed after the process exits**, by its full size, until a reboot. A heap opened from an address on a view of the same kind of section leaves nothing behind.
- **The title does not allocate from a block.** At start-up it reserves two ranges of 24 GiB and later maps physical pages into them where it likes, possibly the same page at two addresses. It also commits pools of 0.5 to 2.2 GiB at addresses it leaves to the system.

## Decisions

- **M1. Physical memory is a set of banks.** A bank is a committed, page-file-backed section of 2 GiB with one view that stays mapped for the bank's life, its anchor. Banks are made when memory is first needed from them. Each is opened for the GPU with `OpenExistingHeapFromAddress` on its anchor, with one buffer over it.
- **M2. A frame is 64 KiB of a bank.** Frames are numbered across banks: bank times 32,768 plus the frame's place in the bank. This is the unit the console's physical-page calls count in, and the unit Windows can map a view at.
- **M3. Every byte the console's memory calls hand out is frames.** `XMemAllocatePhysicalPages` gives frames. `XMemMapPhysicalPages` maps them into a reservation as views of their banks; mapping a frame at a second address is another view of it. `XMemVirtualAlloc` with commit takes frames and maps them at the address it chooses. A reservation without commit is address space only.
- **M4. A title address is found through a page table.** The table says, for each mapped 64 KiB of title addresses, which frame is there. A range of title addresses resolves to a list of runs, each a bank, an offset and a length. This replaces D2's one subtraction.
- **M5. A resource that resolves to one run is used in place; one that resolves to several is promoted.** The GPU addresses a bank's buffer by offset, so a resource has to be contiguous in one bank to be used where it lies. One that is not gets a copy in video memory under D3's rules, which already keep such a copy correct.
- **M6. Frames are handed out to keep runs whole.** A request for several frames gets them contiguous in one bank when any bank has room, so that M5's copy is the exception. A request larger than a bank is contiguous within each bank it spans.
- **M7. A write guard covers every view of its frames.** A frame can be mapped at more than one address, and a write through any of them changes it. The guard protects all of them and any of them ends it.
- **M8. Freed frames give their RAM back at once**, by discarding them through the bank's anchor (D10). A bank's commit charge is taken when the bank is made and returned when the process ends.

## What follows for the other pieces

- **`riptide-mem`** owns banks, frames, the title's address space (reservations, mappings) and the page table, and keeps write guards and the RAM budget. Its one-block allocator goes.
- **`riptide-hle`** turns the console's memory calls into `riptide-mem` calls and keeps only the console's flag rules: page sizes, address limits, the title's allocation hooks.
- **`riptide-gpu`** resolves each resource through the page table when the title creates or binds it, and holds one buffer per bank.
- **`riptide-shader`** reads title memory by bank and offset. Descriptors the title writes hold title addresses, so D5's in-shader decoding needs the page table on the GPU: a buffer indexed by address divided by 64 KiB. How it is laid out is decided with the shader work (Q6); the CPU table is its source.
- **The C runtime's heap is not title memory.** A title's `malloc` that does not go through the console's memory calls is ordinary host memory. If a title hands such a pointer to the GPU, the data is copied at the call. The real title routes its allocations through its own pools, which are frames.

## What is not decided here

- 2 MiB large pages (Q12). Banks use ordinary pages.
- The GPU's copy of the page table (above).
- Whether banks can be made smaller or larger than 2 GiB to advantage. The size is one constant.

## How it is verified

- Unit tests in `riptide-mem`: frames are handed out contiguous and reused after being freed; a frame mapped at two addresses shows a write through one at the other; a range resolves to the runs it was mapped from; a guard hears a write through either address.
- A GPU test on the target machine: a bank opened from its anchor, a CPU write through a second mapping read by the GPU, and a GPU write read by the CPU.
- A measurement on the target machine: committed memory before and after a run that opens banks as heaps, recorded in `docs/measurements.md`.
- The real title's start-up asks for the same memory as before and gets it as frames.
