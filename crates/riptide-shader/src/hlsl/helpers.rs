//! The HLSL helper functions an emitted shader may call.
//!
//! Each operation of the intermediate form whose result differs from what
//! an HLSL operator of the same name gives is written once here, as a small
//! function whose body follows [`crate::ir::eval`] line for line. An emitted
//! shader starts with the helpers it uses and those they call, in the order
//! of [`HELPERS`], which lists every helper after the ones it needs.
//!
//! Float arithmetic is declared `precise`: the compiler then leaves out the
//! fast-math flags that would let a driver reorder it, contract a product
//! and a sum into one operation, or assume that no operand is a NaN. NaNs
//! are told by their bits, never by a float comparison, for the same reason.

/// One helper: its name, the helpers it calls, and its text. `HELPER(x);`
/// in a text marks where a pixel shader's helper lane leaves with `x`
/// (section 7 of the research note); in other stages the line is removed.
pub(crate) struct Helper {
    pub(crate) name: &'static str,
    pub(crate) needs: &'static [&'static str],
    pub(crate) text: &'static str,
}

/// Every helper, each after the helpers it needs.
pub(crate) const HELPERS: &[Helper] = &[
    Helper {
        name: "page_table",
        needs: &[],
        text: "ByteAddressBuffer page_table : register(t0, space0);\n",
    },
    Helper {
        name: "is_nan",
        needs: &[],
        text: "bool is_nan(uint a) { return (a & 0x7fffffffu) > 0x7f800000u; }\n",
    },
    Helper {
        name: "f_ord",
        needs: &["is_nan"],
        text: "\
bool f_ord(uint a, uint b) { return and(!is_nan(a), !is_nan(b)); }
",
    },
    Helper {
        name: "f_add",
        needs: &[],
        text: "uint f_add(uint a, uint b) { precise float r = asfloat(a) + asfloat(b); return asuint(r); }\n",
    },
    Helper {
        name: "f_sub",
        needs: &[],
        text: "uint f_sub(uint a, uint b) { precise float r = asfloat(a) - asfloat(b); return asuint(r); }\n",
    },
    Helper {
        name: "f_mul",
        needs: &[],
        text: "uint f_mul(uint a, uint b) { precise float r = asfloat(a) * asfloat(b); return asuint(r); }\n",
    },
    Helper {
        name: "f_mul_legacy",
        needs: &[],
        text: "\
// Zero times anything is +0, infinity and NaN included.
uint f_mul_legacy(uint a, uint b) {
    precise float r = asfloat(a) * asfloat(b);
    return select(or((a & 0x7fffffffu) == 0u, (b & 0x7fffffffu) == 0u), 0u, asuint(r));
}
",
    },
    Helper {
        name: "f_min",
        needs: &["is_nan"],
        text: "\
// A NaN loses, and -0 is below +0.
uint f_min(uint a, uint b) {
    uint ordered = select(((a | b) & 0x7fffffffu) == 0u, a | b, select(asfloat(a) < asfloat(b), a, b));
    return select(is_nan(a), b, select(is_nan(b), a, ordered));
}
",
    },
    Helper {
        name: "f_max",
        needs: &["is_nan"],
        text: "\
// A NaN loses, and +0 is above -0.
uint f_max(uint a, uint b) {
    uint ordered = select(((a | b) & 0x7fffffffu) == 0u, a & b, select(asfloat(a) > asfloat(b), a, b));
    return select(is_nan(a), b, select(is_nan(b), a, ordered));
}
",
    },
    Helper {
        name: "f_med3",
        needs: &["is_nan", "f_min", "f_max"],
        text: "\
// The median of three; with a NaN among them, their minimum.
uint f_med3(uint a, uint b, uint c) {
    uint largest = f_max(f_max(a, b), c);
    uint median = select(largest == a, f_max(b, c), select(largest == b, f_max(a, c), f_max(a, b)));
    return select(or(is_nan(a), or(is_nan(b), is_nan(c))), f_min(f_min(a, b), c), median);
}
",
    },
    Helper {
        name: "f_saturate",
        needs: &["is_nan"],
        text: "\
// Clamps to 0..1; a NaN gives 0.
uint f_saturate(uint a) {
    float x = asfloat(a);
    return select(or(is_nan(a), x < 0.0), 0u, select(x > 1.0, 0x3f800000u, a));
}
",
    },
    Helper {
        name: "f_rcp",
        needs: &[],
        text: "uint f_rcp(uint a) { precise float r = 1.0 / asfloat(a); return asuint(r); }\n",
    },
    Helper {
        name: "f_rsqrt",
        needs: &[],
        text: "uint f_rsqrt(uint a) { precise float r = rsqrt(asfloat(a)); return asuint(r); }\n",
    },
    Helper {
        name: "f_sqrt",
        needs: &[],
        text: "uint f_sqrt(uint a) { precise float r = sqrt(asfloat(a)); return asuint(r); }\n",
    },
    Helper {
        name: "f_exp2",
        needs: &[],
        text: "uint f_exp2(uint a) { precise float r = exp2(asfloat(a)); return asuint(r); }\n",
    },
    Helper {
        name: "f_log2",
        needs: &[],
        text: "uint f_log2(uint a) { precise float r = log2(asfloat(a)); return asuint(r); }\n",
    },
    Helper {
        name: "f_turns",
        needs: &["is_nan"],
        text: "\
// Sine or cosine of an angle in turns: exact at the quarter turns, and 0 or
// 1 outside -256..256. The whole turns are taken off first, which is exact,
// so that the host's sine sees an angle of at most half a turn.
uint f_turns(uint a, bool cosine) {
    float x = asfloat(a);
    bool inside = and(x >= -256.0, x <= 256.0);
    precise float turn = x - round(x);
    precise float quarter = turn * 4.0;
    uint place = ((uint)(int)quarter + select(cosine, 1u, 0u)) & 3u;
    uint exact = select(place == 1u, 0x3f800000u, select(place == 3u, 0xbf800000u, 0u));
    precise float angle = turn * 6.28318530717958647692;
    precise float r = select(cosine, cos(angle), sin(angle));
    uint value = select(quarter == round(quarter), exact, asuint(r));
    return select(is_nan(a), a, select(inside, value, select(cosine, 0x3f800000u, 0u)));
}
",
    },
    Helper {
        name: "f_fract",
        needs: &["is_nan"],
        text: "\
// x - floor(x), but never 1.0.
uint f_fract(uint a) {
    float x = asfloat(a);
    precise float r = x - floor(x);
    return select(and(!is_nan(asuint(r)), r >= 1.0), 0x3f7fffffu, asuint(r));
}
",
    },
    Helper {
        name: "f_from_u",
        needs: &[],
        text: "// An unsigned integer to a float, rounded once: each half converts exactly.
uint f_from_u(uint a) {
    precise float r = (float)(a >> 16) * 65536.0 + (float)(a & 0xffffu);
    return asuint(r);
}
",
    },
    Helper {
        name: "f_to_i",
        needs: &["is_nan"],
        text: "\
// Toward zero, saturating; a NaN gives 0.
uint f_to_i(uint a) {
    float x = asfloat(a);
    uint inside = select(x >= 2147483648.0, 0x7fffffffu, select(x <= -2147483648.0, 0x80000000u, (uint)(int)x));
    return select(is_nan(a), 0u, inside);
}
",
    },
    Helper {
        name: "f_to_u",
        needs: &["is_nan"],
        text: "\
// Toward zero, saturating; negative values and a NaN give 0.
uint f_to_u(uint a) {
    float x = asfloat(a);
    uint inside = select(x >= 4294967296.0, 0xffffffffu, select(x <= 0.0, 0u, (uint)x));
    return select(is_nan(a), 0u, inside);
}
",
    },
    Helper {
        name: "half_from_f",
        needs: &[],
        text: "\
// A 32-bit float to the bits of a 16-bit float, to nearest even or toward
// zero.
uint half_from_f(uint bits, bool toward_zero) {
    uint sign = (bits >> 16) & 0x8000u;
    int exponent = (int)((bits >> 23) & 0xffu);
    uint fraction = bits & 0x7fffffu;
    // Infinity, or a NaN, which keeps its top fraction bits.
    uint special = sign | 0x7c00u | select(fraction == 0u, 0u, 0x200u | (fraction >> 13));
    // Too large: infinity, or the largest finite value toward zero.
    uint large = sign | select(toward_zero, 0x7bffu, 0x7c00u);
    int unbiased = exponent - 127;
    bool normal = unbiased >= -14;
    // How many low bits are dropped: 13 for a normal half, more for a
    // denormal one; 25 stands for \"all of them\".
    uint shift = (uint)clamp(select(normal, 13, -1 - unbiased), 1, 25);
    uint significand = fraction | 0x800000u;
    uint kept = significand >> shift;
    uint dropped = significand & ((1u << shift) - 1u);
    uint halfway = 1u << (shift - 1u);
    uint result = select(normal, ((uint)(unbiased + 14) << 10) + kept, kept);
    bool up = and(!toward_zero, or(dropped > halfway, and(dropped == halfway, (result & 1u) != 0u)));
    uint finite = sign | ((result + select(up, 1u, 0u)) & 0xffffu);
    bool nothing = or(exponent == 0, shift > 24u);
    return select(exponent == 255, special, select(unbiased > 15, large, select(nothing, sign, finite)));
}
",
    },
    Helper {
        name: "f_from_half",
        needs: &[],
        text: "\
// The 16-bit float in the low half of a word to a 32-bit float.
uint f_from_half(uint a) {
    uint half = a & 0xffffu;
    uint sign = (half & 0x8000u) << 16;
    uint exponent = (half >> 10) & 0x1fu;
    uint fraction = half & 0x3ffu;
    // A denormal: the fraction moves up until its leading one is hidden.
    uint shift = 10u - firstbithigh(max(fraction, 1u));
    uint denormal = sign | ((113u - shift) << 23) | (((fraction << shift) & 0x3ffu) << 13);
    uint small = select(fraction == 0u, sign, denormal);
    uint special = sign | 0x7f800000u | (fraction << 13);
    uint normal = sign | ((exponent + 112u) << 23) | (fraction << 13);
    return select(exponent == 0u, small, select(exponent == 31u, special, normal));
}
",
    },
    Helper {
        name: "mul_hi_u",
        needs: &[],
        text: "\
// The high 32 bits of the unsigned product.
uint mul_hi_u(uint a, uint b) {
    uint a0 = a & 0xffffu, a1 = a >> 16, b0 = b & 0xffffu, b1 = b >> 16;
    uint m1 = a1 * b0 + ((a0 * b0) >> 16);
    uint m2 = a0 * b1 + (m1 & 0xffffu);
    return a1 * b1 + (m1 >> 16) + (m2 >> 16);
}
",
    },
    Helper {
        name: "mul_hi_s",
        needs: &["mul_hi_u"],
        text: "\
// The high 32 bits of the signed product.
uint mul_hi_s(uint a, uint b) {
    return mul_hi_u(a, b) - select((int)a < 0, b, 0u) - select((int)b < 0, a, 0u);
}
",
    },
    Helper {
        name: "f_scale",
        needs: &[],
        text: "\
// A float times a power of two, in steps that are each exact.
uint f_scale(uint a, uint b) {
    int e = clamp((int)b, -400, 400);
    precise float r = asfloat(a);
    int step = clamp(e, -100, 100);
    r = r * asfloat((uint)(step + 127) << 23);
    e -= step;
    step = clamp(e, -100, 100);
    r = r * asfloat((uint)(step + 127) << 23);
    e -= step;
    step = clamp(e, -100, 100);
    r = r * asfloat((uint)(step + 127) << 23);
    e -= step;
    r = r * asfloat((uint)(e + 127) << 23);
    return asuint(r);
}
",
    },
    Helper {
        name: "f_fused",
        needs: &[],
        text: "\
// a * b + c. The host decides whether the product is rounded before the
// sum; the console rounds once.
uint f_fused(uint a, uint b, uint c) { precise float r = mad(asfloat(a), asfloat(b), asfloat(c)); return asuint(r); }
",
    },
    Helper {
        name: "f_mul_add",
        needs: &[],
        text: "\
// a * b + c with the product rounded before the sum.
uint f_mul_add(uint a, uint b, uint c) {
    precise float p = asfloat(a) * asfloat(b);
    precise float r = p + asfloat(c);
    return asuint(r);
}
",
    },
    Helper {
        name: "f_class",
        needs: &[],
        text: "\
// Whether a float is of one of the classes whose bits are set.
bool f_class(uint value, uint classes) {
    bool negative = (value >> 31) != 0u;
    uint exponent = (value >> 23) & 0xffu;
    uint fraction = value & 0x7fffffu;
    uint special = select(fraction == 0u, select(negative, 2u, 9u), select((fraction >> 22) != 0u, 1u, 0u));
    uint small = select(fraction == 0u, select(negative, 5u, 6u), select(negative, 4u, 7u));
    uint kind = select(exponent == 255u, special, select(exponent == 0u, small, select(negative, 3u, 8u)));
    return ((classes >> kind) & 1u) != 0u;
}
",
    },
    Helper {
        name: "carry",
        needs: &[],
        text: "\
bool carry(uint a, uint b, bool carry_in) {
    uint sum = a + b;
    return or(sum < a, sum + select(carry_in, 1u, 0u) < sum);
}
",
    },
    Helper {
        name: "borrow",
        needs: &[],
        text: "\
bool borrow(uint a, uint b, bool borrow_in) { return select(borrow_in, a <= b, a < b); }
",
    },
    Helper {
        name: "perm_bytes",
        needs: &[],
        text: "\
// One byte of the console's byte permutation: b is the low four bytes.
uint perm_byte(uint a, uint b, uint pick) {
    uint by = (pick & 3u) * 8u;
    uint low = (b >> by) & 0xffu;
    uint high = (a >> by) & 0xffu;
    // 8 to 11 repeat the top bit of byte 1, 3, 5, 7.
    uint odd = ((pick & 1u) * 2u + 1u) * 8u;
    uint sign = select(((select(pick < 10u, b, a) >> odd) & 0x80u) != 0u, 0xffu, 0u);
    return select(pick < 4u, low, select(pick < 8u, high, select(pick < 12u, sign, select(pick == 12u, 0u, 0xffu))));
}
uint perm_bytes(uint a, uint b, uint c) {
    return perm_byte(a, b, c & 0xffu) | (perm_byte(a, b, (c >> 8) & 0xffu) << 8)
        | (perm_byte(a, b, (c >> 16) & 0xffu) << 16) | (perm_byte(a, b, c >> 24) << 24);
}
",
    },
    Helper {
        name: "cube",
        needs: &[],
        text: "\
// The major axis of a cube direction: z, else y, else x.
uint cube_axis(uint a, uint b, uint c) {
    float x = abs(asfloat(a)), y = abs(asfloat(b)), z = abs(asfloat(c));
    return select(and(z >= x, z >= y), 2u, select(y >= x, 1u, 0u));
}
uint cube_face(uint a, uint b, uint c) {
    uint axis = cube_axis(a, b, c);
    uint value = select(axis == 2u, c, select(axis == 1u, b, a));
    float face = (float)(axis * 2u) + select(asfloat(value) < 0.0, 1.0, 0.0);
    return asuint(face);
}
uint cube_s(uint a, uint b, uint c) {
    uint axis = cube_axis(a, b, c);
    uint along_z = select(asfloat(c) < 0.0, a ^ 0x80000000u, a);
    uint along_x = select(asfloat(a) < 0.0, c, c ^ 0x80000000u);
    return select(axis == 2u, along_z, select(axis == 1u, a, along_x));
}
uint cube_t(uint a, uint b, uint c) {
    uint axis = cube_axis(a, b, c);
    return select(axis == 1u, select(asfloat(b) < 0.0, c ^ 0x80000000u, c), b ^ 0x80000000u);
}
uint cube_major(uint a, uint b, uint c) {
    uint axis = cube_axis(a, b, c);
    precise float r = 2.0 * asfloat(select(axis == 2u, c, select(axis == 1u, b, a)));
    return asuint(r);
}
",
    },
    Helper {
        name: "mem_load",
        needs: &["page_table"],
        text: "\
// The word of title memory that holds the byte at an address given as two
// halves. Zero where nothing is mapped or beyond what the page table covers.
// Written without branches, as every helper that is called often is: the
// compiler's time goes up steeply with the number of branches in a program.
// Where there is nothing to read, the first word of bank 0 is read and
// thrown away.
uint mem_word(uint low, uint high) {
    bool covered = high < 32u;
    uint entry = page_table.Load(select(covered, ((low >> 16) | (high << 16)) * 4u, 0u));
    uint frame = entry - 1u;
    bool mapped = and(covered, and(entry != 0u, (frame >> 15) < 16u));
    ByteAddressBuffer bank = ResourceDescriptorHeap[select(mapped, frame >> 15, 0u)];
    uint value = bank.Load(select(mapped, ((frame & 0x7fffu) << 16) | (low & 0xfffcu), 0u));
    return select(mapped, value, 0u);
}
// Four bytes at any address: a load that is not aligned takes two words,
// each found through the page table by itself, so it may cross a page.
uint mem_load32(uint low, uint high) {
    uint first = mem_word(low, high);
    uint shift = (low & 3u) * 8u;
    uint next = (low & 0xfffffffcu) + 4u;
    uint second = mem_word(next, high + select(next == 0u, 1u, 0u));
    return select(shift == 0u, first, (first >> shift) | (second << ((32u - shift) & 31u)));
}
// Four bytes at an address from the two words of memory that hold them.
uint mem_pick(uint low, uint first, uint second) {
    uint shift = (low & 3u) * 8u;
    return select(shift == 0u, first, (first >> shift) | (second << ((32u - shift) & 31u)));
}
// The same where the address is known to be a multiple of four.
uint mem_load32_aligned(uint low, uint high) { return mem_word(low, high); }
uint mem_load16(uint low, uint high) { return mem_load32(low, high) & 0xffffu; }
uint mem_load8(uint low, uint high) { return (mem_word(low, high) >> ((low & 3u) * 8u)) & 0xffu; }
// The same at an offset from an address, with the carry into the high half.
uint mem_load32_at(uint low, uint high, uint offset) { uint sum = low + offset; return mem_load32(sum, high + select(sum < low, 1u, 0u)); }
uint mem_load32_aligned_at(uint low, uint high, uint offset) { uint sum = low + offset; return mem_word(sum, high + select(sum < low, 1u, 0u)); }
uint mem_load16_at(uint low, uint high, uint offset) { uint sum = low + offset; return mem_load16(sum, high + select(sum < low, 1u, 0u)); }
uint mem_load8_at(uint low, uint high, uint offset) { uint sum = low + offset; return mem_load8(sum, high + select(sum < low, 1u, 0u)); }
",
    },
    Helper {
        name: "mem_store",
        needs: &["page_table"],
        text: "\
// Writes the bits of an aligned word of title memory that `mask` names.
// Dropped where nothing is mapped. A part of a word is written with two
// interlocked operations, so that lanes writing other bytes of the same
// word at the same time do not undo each other.
void mem_store_word(uint low, uint high, uint value, uint mask) {
    bool covered = high < 32u;
    uint entry = page_table.Load(select(covered, ((low >> 16) | (high << 16)) * 4u, 0u));
    uint frame = entry - 1u;
    if (and(and(mask != 0u, covered), and(entry != 0u, (frame >> 15) < 16u))) {
        RWByteAddressBuffer bank = ResourceDescriptorHeap[16u + (frame >> 15)];
        uint at = ((frame & 0x7fffu) << 16) | (low & 0xfffcu);
        if (mask == 0xffffffffu) {
            bank.Store(at, value);
        } else {
            uint before;
            bank.InterlockedAnd(at, ~mask, before);
            bank.InterlockedOr(at, value & mask, before);
        }
    }
}
// Writes the low bits of `value` that `mask` names at any address.
void mem_store(uint low, uint high, uint value, uint mask) {
    uint shift = (low & 3u) * 8u;
    mem_store_word(low, high, value << shift, mask << shift);
    if (shift != 0u) {
        uint next = (low & 0xfffffffcu) + 4u;
        mem_store_word(next, high + select(next == 0u, 1u, 0u), value >> (32u - shift), mask >> (32u - shift));
    }
}
void mem_store_at(uint low, uint high, uint offset, uint value, uint mask) { uint sum = low + offset; mem_store(sum, high + select(sum < low, 1u, 0u), value, mask); }
",
    },
    Helper {
        name: "buf_locate",
        needs: &["mul_hi_u"],
        text: "\
// Where a unit of a buffer lies, from the buffer's descriptor: the two
// halves of its title address, and whether it is inside the buffer by the
// descriptor's own bound check.
uint3 buf_locate(uint4 d, uint index, uint offset) {
    uint stride = (d.y >> 16) & 0x3fffu;
    uint records = d.z;
    uint check = (d.w >> 28) & 3u;
    uint position = index * stride + offset;
    uint position_high = mul_hi_u(index, stride) + select(position < offset, 1u, 0u);
    bool past = index >= records;
    bool in_bytes = or(position_high != 0u, position >= records);
    bool outside = select(check == 0u, or(past, offset >= stride), select(check == 1u, past, select(check == 2u, records == 0u, in_bytes)));
    uint low = d.x + position;
    uint high = (d.y & 0xffu) + position_high + select(low < position, 1u, 0u);
    return uint3(low, high, select(outside, 0u, 1u));
}
",
    },
    Helper {
        name: "buf_load",
        needs: &["buf_locate", "mem_load"],
        text: "\
uint buf_load32(uint4 d, uint index, uint offset) { uint3 at = buf_locate(d, index, offset); return select(at.z != 0u, mem_load32(at.x, at.y), 0u); }
uint buf_load16(uint4 d, uint index, uint offset) { uint3 at = buf_locate(d, index, offset); return select(at.z != 0u, mem_load16(at.x, at.y), 0u); }
uint buf_load8(uint4 d, uint index, uint offset) { uint3 at = buf_locate(d, index, offset); return select(at.z != 0u, mem_load8(at.x, at.y), 0u); }
// Four bytes at an address from the two words of memory that hold them,
// or zero outside the buffer.
uint buf_pick(uint inside, uint low, uint first, uint second) { return select(inside != 0u, mem_pick(low, first, second), 0u); }
",
    },
    Helper {
        name: "buf_store",
        needs: &["buf_locate", "mem_store"],
        text: "\
void buf_store(uint4 d, uint index, uint offset, uint value, uint mask) {
    uint3 at = buf_locate(d, index, offset);
    if (at.z != 0u) mem_store(at.x, at.y, value, mask);
}
",
    },
    Helper {
        name: "fmt_decode",
        needs: &["f_from_half"],
        text: "\
// Bits `low` to `low + width - 1` of an element given as four words.
uint fmt_bits(uint4 raw, uint low, uint width) {
    uint word = low >> 5, shift = low & 31u;
    uint value = raw[word] >> shift;
    if (shift != 0u && word < 3u) value |= raw[word + 1u] << (32u - shift);
    return width >= 32u ? value : value & ((1u << width) - 1u);
}
// An unsigned float of 10 or 11 bits.
uint fmt_small_float(uint code, uint fraction_bits) {
    uint exponent = (code >> fraction_bits) & 0x1fu;
    uint fraction = code & ((1u << fraction_bits) - 1u);
    float scale = (float)(1u << fraction_bits);
    if (exponent == 31u) return fraction == 0u ? 0x7f800000u : 0x7fc00000u;
    if (exponent == 0u) {
        precise float tiny = (float)fraction / scale * asfloat(0x38800000u);
        return asuint(tiny);
    }
    precise float r = (1.0 + (float)fraction / scale) * asfloat((exponent + 112u) << 23);
    return asuint(r);
}
// One channel of a buffer format: kind 0 unorm, 1 snorm, 2 uscaled,
// 3 sscaled, 4 uint, 5 sint, 6 float.
uint fmt_channel(uint code, uint bits, uint kind) {
    uint largest = bits >= 32u ? 0xffffffffu : (1u << bits) - 1u;
    int extended = (int)(code << (32u - bits)) >> (32u - bits);
    if (kind == 0u) {
        precise float r = (float)code / (float)largest;
        return asuint(r);
    }
    if (kind == 1u) {
        precise float r = (float)extended / (float)(largest >> 1);
        return r < -1.0 ? 0xbf800000u : asuint(r);
    }
    if (kind == 2u) return asuint((float)code);
    if (kind == 3u) return asuint((float)extended);
    if (kind == 4u) return code;
    if (kind == 5u) return (uint)extended;
    if (bits == 32u) return code;
    if (bits == 16u) return f_from_half(code);
    return fmt_small_float(code, bits - 5u);
}
// The four channels of an element. `info.x` holds the widths of x, y, z
// and w, a byte each; `info.y` the kind. A channel the format lacks reads
// 0, and w reads one.
uint4 fmt_decode(uint4 raw, uint2 info) {
    bool integer = info.y == 4u || info.y == 5u;
    uint4 result = uint4(0u, 0u, 0u, integer ? 1u : 0x3f800000u);
    uint low = 0u;
    for (uint channel = 0u; channel < 4u; channel++) {
        uint bits = (info.x >> (channel * 8u)) & 0xffu;
        if (bits != 0u) {
            result[channel] = fmt_channel(fmt_bits(raw, low, bits), bits, info.y);
            low += bits;
        }
    }
    return result;
}
",
    },
    // The table of buffer formats is written from `eval::BufferFormat` when
    // a shader is emitted; this entry only gives it its place.
    Helper {
        name: "fmt_info",
        needs: &[],
        text: "",
    },
    Helper {
        name: "fmt_load",
        needs: &["fmt_info", "fmt_decode", "buf_locate", "mem_load"],
        text: "\
// Reads one element of a typed buffer. Zeroes outside the buffer.
uint4 fmt_load(uint4 d, uint index, uint offset, uint format) {
    uint2 info = fmt_info(format);
    uint3 at = buf_locate(d, index, offset);
    if (at.z == 0u || info.x == 0u) return uint4(0u, 0u, 0u, 0u);
    uint bytes = ((info.x & 0xffu) + ((info.x >> 8) & 0xffu) + ((info.x >> 16) & 0xffu) + (info.x >> 24)) >> 3;
    uint4 raw = uint4(mem_load32(at.x, at.y), 0u, 0u, 0u);
    if (bytes > 4u) raw.y = mem_load32_at(at.x, at.y, 4u);
    if (bytes > 8u) raw.z = mem_load32_at(at.x, at.y, 8u);
    if (bytes > 12u) raw.w = mem_load32_at(at.x, at.y, 12u);
    return fmt_decode(raw, info);
}
",
    },
    Helper {
        name: "fmt_store",
        needs: &[
            "fmt_info",
            "buf_locate",
            "mem_store",
            "is_nan",
            "f_to_i",
            "f_to_u",
            "half_from_f",
        ],
        text: "\
// One channel of a buffer format from a word, the inverse of fmt_channel.
// The formats with 10-bit and 11-bit floats are not written: they give 0.
uint fmt_encode(uint value, uint bits, uint kind) {
    uint field = bits >= 32u ? 0xffffffffu : (1u << bits) - 1u;
    float largest = (float)(field >> 1);
    float x = is_nan(value) ? 0.0 : asfloat(value);
    if (kind == 0u) {
        precise float scaled = clamp(x, 0.0, 1.0) * (float)field;
        return (uint)round(scaled);
    }
    if (kind == 1u) {
        precise float scaled = clamp(x, -1.0, 1.0) * largest;
        return (uint)(int)round(scaled) & field;
    }
    if (kind == 2u) return min(f_to_u(value), field);
    if (kind == 3u) return (uint)clamp((int)f_to_i(value), -(int)largest - 1, (int)largest) & field;
    if (kind == 4u || kind == 5u) return value & field;
    if (bits == 32u) return value;
    if (bits == 16u) return half_from_f(value, false);
    return 0u;
}
// Writes one element of a typed buffer. Dropped outside the buffer.
void fmt_store(uint4 d, uint index, uint offset, uint format, uint4 values) {
    uint2 info = fmt_info(format);
    uint3 at = buf_locate(d, index, offset);
    if (at.z == 0u || info.x == 0u) return;
    uint4 raw = uint4(0u, 0u, 0u, 0u);
    uint low = 0u;
    for (uint channel = 0u; channel < 4u; channel++) {
        uint bits = (info.x >> (channel * 8u)) & 0xffu;
        if (bits != 0u) {
            uint code = fmt_encode(values[channel], bits, info.y);
            uint word = low >> 5, shift = low & 31u;
            raw[word] |= code << shift;
            if (shift + bits > 32u) raw[word + 1u] |= code >> (32u - shift);
            low += bits;
        }
    }
    uint bytes = low >> 3;
    for (uint part = 0u; part < 4u; part++) {
        if (bytes > part * 4u) {
            uint count = min(bytes - part * 4u, 4u);
            uint mask = count >= 4u ? 0xffffffffu : (1u << (count * 8u)) - 1u;
            mem_store_at(at.x, at.y, part * 4u, raw[part], mask);
        }
    }
}
",
    },
    Helper {
        name: "ballot_first",
        needs: &[],
        text: "\
// The number of the first lane of a ballot, or all ones for none.
uint ballot_first(uint4 ballot) {
    uint high = select(ballot.z != 0u, 64u + firstbitlow(ballot.z), select(ballot.w != 0u, 96u + firstbitlow(ballot.w), 0xffffffffu));
    return select(ballot.x != 0u, firstbitlow(ballot.x), select(ballot.y != 0u, 32u + firstbitlow(ballot.y), high));
}
",
    },
    Helper {
        name: "wave_first",
        needs: &["ballot_first"],
        text: "\
// The value of the first lane whose flag holds; with none, of the first
// lane there is.
uint wave_first(uint value, bool active) {
    HELPER(value);
    uint lane = ballot_first(WaveActiveBallot(active));
    uint found = WaveReadLaneAt(value, min(lane, WaveGetLaneCount() - 1u));
    return select(lane == 0xffffffffu, WaveReadLaneFirst(value), found);
}
",
    },
    Helper {
        name: "wave_read",
        needs: &[],
        text: "\
// The value of the lane with a number; zero if that lane is not here.
uint wave_read(uint value, uint lane) {
    HELPER(select(lane == WaveGetLaneIndex(), value, 0u));
    uint4 here = WaveActiveBallot(true);
    uint safe = min(lane, WaveGetLaneCount() - 1u);
    bool there = and(lane == safe, ((here[safe >> 5] >> (safe & 31u)) & 1u) != 0u);
    return select(there, WaveReadLaneAt(value, safe), 0u);
}
",
    },
    Helper {
        name: "wave_count",
        needs: &[],
        text: "\
uint wave_count(bool flag) {
    HELPER(select(flag, 1u, 0u));
    return WaveActiveCountBits(flag);
}
",
    },
    Helper {
        name: "wave_any",
        needs: &[],
        text: "\
bool wave_any(bool flag) {
    HELPER(flag);
    return WaveActiveAnyTrue(flag);
}
",
    },
    Helper {
        name: "wave_first_index",
        needs: &["ballot_first"],
        text: "\
uint wave_first_index(bool flag) {
    HELPER(select(flag, WaveGetLaneIndex(), 0xffffffffu));
    return ballot_first(WaveActiveBallot(flag));
}
",
    },
    Helper {
        name: "wave_below",
        needs: &[],
        text: "\
// How many lanes before this one hold the flag: all of them (half 0), or
// those of lanes 0 to 31 (1) or 32 to 63 (2).
uint wave_below(bool flag, uint half) {
    HELPER(0u);
    bool counted = and(flag, or(half == 0u, (WaveGetLaneIndex() >= 32u) == (half == 2u)));
    return WavePrefixCountBits(counted);
}
",
    },
    Helper {
        name: "wave_ballot",
        needs: &[],
        text: "\
// The flags of lanes 0 to 31, or 32 to 63, as a word.
uint wave_ballot(bool flag, bool high) {
    uint lane = WaveGetLaneIndex();
    HELPER(select(and(flag, and((lane >= 32u) == high, lane < 64u)), 1u << (lane & 31u), 0u));
    uint4 ballot = WaveActiveBallot(flag);
    return select(high, ballot.y, ballot.x);
}
",
    },
    Helper {
        name: "lane_bit",
        needs: &[],
        text: "\
// The lane's own bit of a 64-bit number.
bool lane_bit(uint low, uint high) {
    uint lane = WaveGetLaneIndex();
    uint half = select(lane < 32u, low, high);
    return and(lane < 64u, ((half >> (lane & 31u)) & 1u) != 0u);
}
",
    },
    Helper {
        name: "quad_read",
        needs: &[],
        text: "\
// The value of another lane of the quad: 0 this lane, 1 the one beside it,
// 2 the one below or above, 3 the one across.
uint quad_read(uint value, uint across) {
    uint x = QuadReadAcrossX(value);
    uint y = QuadReadAcrossY(value);
    uint d = QuadReadAcrossDiagonal(value);
    return select(across == 0u, value, select(across == 1u, x, select(across == 2u, y, d)));
}
",
    },
    Helper {
        name: "quad_any",
        needs: &[],
        text: "\
bool quad_any(bool flag) {
    uint own = select(flag, 1u, 0u);
    return (own | QuadReadAcrossX(own) | QuadReadAcrossY(own) | QuadReadAcrossDiagonal(own)) != 0u;
}
",
    },
    Helper {
        name: "cube_direction",
        needs: &[],
        text: "\
// The direction a cube access means, from the two face coordinates (each
// 1 to 2 across the face, as the console's code computes them) and the
// face number.
float3 cube_direction(float s, float t, float face) {
    s = 2.0 * (s - 1.5);
    t = 2.0 * (t - 1.5);
    uint which = (uint)clamp(face, 0.0, 5.0) % 6u;
    if (which == 0u) return float3(1.0, -t, -s);
    if (which == 1u) return float3(-1.0, -t, s);
    if (which == 2u) return float3(s, 1.0, t);
    if (which == 3u) return float3(s, -1.0, -t);
    if (which == 4u) return float3(s, -t, 1.0);
    return float3(-s, -t, -1.0);
}
// The same for a change of the two face coordinates.
float3 cube_change(float s, float t, float face) {
    s = 2.0 * s;
    t = 2.0 * t;
    uint which = (uint)clamp(face, 0.0, 5.0) % 6u;
    if (which == 0u) return float3(0.0, -t, -s);
    if (which == 1u) return float3(0.0, -t, s);
    if (which == 2u) return float3(s, 0.0, t);
    if (which == 3u) return float3(s, 0.0, -t);
    if (which == 4u) return float3(s, -t, 0.0);
    return float3(-s, -t, 0.0);
}
",
    },
    Helper {
        name: "texel_offset",
        needs: &[],
        text: "\
// The three signed 6-bit numbers of a texel offset word.
int3 texel_offset(uint word) {
    return int3((int)(word << 26) >> 26, (int)(word << 18) >> 26, (int)(word << 10) >> 26);
}
",
    },
];
