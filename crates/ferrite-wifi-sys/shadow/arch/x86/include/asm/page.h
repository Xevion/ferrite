/* SPDX-License-Identifier: GPL-2.0 */
/*
 * struct page lookups that never compute an address from vmemmap_base.
 *
 * Linux finds a page struct as vmemmap_base + pfn, and the address of a page
 * from page_offset_base. The shim has no vmemmap: a zero base under UEFI's
 * identity map would turn a refcount update into a write into real RAM. So the
 * shim owns the page structs of its own allocations:
 *
 *   struct page *lx_virt_to_page(const void *addr)   the page struct of the
 *                                                    page holding addr
 *
 * and each page struct records its address (WANT_PAGE_VIRTUAL), which
 * page_address and page_to_virt read back.
 */
#ifndef _LX_SHADOW_ASM_PAGE_H
#define _LX_SHADOW_ASM_PAGE_H

#define WANT_PAGE_VIRTUAL

#include_next <asm/page.h>

#ifndef __ASSEMBLER__

extern struct page *lx_virt_to_page(const void *addr);

#undef virt_to_page
#define virt_to_page(kaddr) lx_virt_to_page((const void *)(kaddr))

#define page_to_virt(page) ((page)->virtual)

#undef pfn_to_page
#undef page_to_pfn
#define pfn_to_page(pfn) lx_virt_to_page(pfn_to_kaddr(pfn))
#define page_to_pfn(page) (__pa(page_to_virt(page)) >> PAGE_SHIFT)

#endif /* !__ASSEMBLER__ */

#endif
