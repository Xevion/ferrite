// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces mm/vmalloc.c. There is no second mapping: a vmalloc block is a
 * physically contiguous compound block from the page allocator, typed
 * large_kmalloc so kfree, kvfree and vfree all free it, and no address is a
 * vmalloc address. Contiguous is stricter than vmalloc promises, so callers
 * are unaffected; a large request just needs a free block of its order.
 */

#include <linux/mm.h>
#include <linux/slab.h>
#include <linux/vmalloc.h>

extern void lx_mem_fault(const char *what, unsigned long addr) __attribute__((__noreturn__));

void *__vmalloc_noprof(unsigned long size, gfp_t gfp)
{
	return size ? __kmalloc_large_noprof(size, gfp) : NULL;
}
EXPORT_SYMBOL(__vmalloc_noprof);

void *vmalloc_noprof(unsigned long size)
{
	return __vmalloc_noprof(size, GFP_KERNEL);
}
EXPORT_SYMBOL(vmalloc_noprof);

void *vzalloc_noprof(unsigned long size)
{
	return __vmalloc_noprof(size, GFP_KERNEL | __GFP_ZERO);
}
EXPORT_SYMBOL(vzalloc_noprof);

void vfree(const void *addr)
{
	struct folio *folio;

	if (!addr)
		return;
	folio = virt_to_folio(addr);
	if (!folio_test_large_kmalloc(folio) || addr != folio_address(folio))
		lx_mem_fault("vfree of a pointer vmalloc did not return", (unsigned long)addr);
	kfree(addr);
}
EXPORT_SYMBOL(vfree);

struct page *vmalloc_to_page(const void *addr)
{
	return virt_to_page(addr);
}
EXPORT_SYMBOL(vmalloc_to_page);

bool is_vmalloc_addr(const void *x)
{
	return false;
}
EXPORT_SYMBOL(is_vmalloc_addr);

int is_vmalloc_or_module_addr(const void *x)
{
	return 0;
}
