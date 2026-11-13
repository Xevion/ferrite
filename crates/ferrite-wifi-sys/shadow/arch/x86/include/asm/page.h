/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The stock flat page model over the shim's arena (ferrite-lkpi mem), with a
 * checked virt_to_page.
 *
 * __pa is virt - page_offset_base, the arena's bus address, and pfn_to_page
 * is vmemmap_base + pfn into the memmap at the arena's front, so page_address,
 * pfn_to_page, page_to_pfn and nth_page are the stock arithmetic. virt_to_page
 * stops the program, through lx_mem_fault, for an address outside the arena
 * instead of indexing past the memmap; the check is on in every build.
 */
#ifndef _LX_SHADOW_ASM_PAGE_H
#define _LX_SHADOW_ASM_PAGE_H

#include_next <asm/page.h>

#ifndef __ASSEMBLER__

extern unsigned long lx_arena_start;
extern unsigned long lx_arena_size;
extern void lx_mem_fault(const char *what, unsigned long addr) __attribute__((__noreturn__));

static __always_inline unsigned long lx_arena_pfn(unsigned long addr)
{
	if (__builtin_expect(addr - lx_arena_start >= lx_arena_size, 0))
		lx_mem_fault("virt_to_page outside the kernel arena", addr);
	return __pa(addr) >> PAGE_SHIFT;
}

#undef virt_to_page
#define virt_to_page(kaddr) pfn_to_page(lx_arena_pfn((unsigned long)(kaddr)))

#endif /* !__ASSEMBLER__ */

#endif
