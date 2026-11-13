// SPDX-License-Identifier: GPL-2.0
/*
 * The page struct of any page the C looks up, made on first use as Genode's
 * lx_emul_virt_to_page associates them. Each records its address
 * (WANT_PAGE_VIRTUAL) and holds one reference, the allocator's, so a
 * get/put pair never frees it. Structs of adjacent pages are not adjacent.
 */

#include <linux/mm.h>
#include <linux/page_ref.h>
#include <linux/slab.h>

extern struct page *lx_page_lookup(unsigned long virt);
extern void lx_page_insert(unsigned long virt, struct page *page);

struct page *lx_virt_to_page(const void *addr)
{
	unsigned long virt = (unsigned long)addr & PAGE_MASK;
	struct page *page = lx_page_lookup(virt);

	if (page)
		return page;

	page = kzalloc(sizeof(*page), GFP_ATOMIC);
	BUG_ON(!page);
	page->virtual = (void *)virt;
	init_page_count(page);
	lx_page_insert(virt, page);
	return page;
}
