// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces mm/page_alloc.c: the page allocator over the shim's buddy
 * allocator (ferrite-lkpi mem), with Linux's page state in the memmap: a
 * refcount, and for __GFP_COMP blocks the compound head, tails and folio
 * order of prep_compound_page. Adapted from Genode's lx_emul shadow (Stefan
 * Kalkowski, Christian Helmuth, Genode Labs).
 *
 * GFP: __GFP_ZERO zeroes the block and __GFP_COMP makes it compound;
 * __GFP_NOWARN silences the failure message. Zone modifiers (__GFP_DMA32 and
 * friends) are ignored because the one arena already sits where every zone
 * would, and reclaim, retry and __GFP_NOFAIL flags because there is nothing to
 * reclaim: an exhausted arena stays exhausted.
 */

#include <linux/gfp.h>
#include <linux/mm.h>
#include <linux/page_ref.h>
#include <linux/printk.h>
#include <linux/static_key.h>

#include "internal.h"

extern struct page *lx_mem_setup(unsigned long page_struct_size);
extern unsigned long lx_page_alloc(unsigned int order);
extern void lx_page_free(unsigned long virt, unsigned int order);
extern void lx_page_split(unsigned long virt, unsigned int order);
extern bool lx_page_is_free(unsigned long virt);

DEFINE_STATIC_KEY_MAYBE(CONFIG_INIT_ON_ALLOC_DEFAULT_ON, init_on_alloc);
EXPORT_SYMBOL(init_on_alloc);

static bool memmap_ready;

void prep_compound_page(struct page *page, unsigned int order)
{
	int i;
	int nr_pages = 1 << order;

	__SetPageHead(page);
	for (i = 1; i < nr_pages; i++)
		prep_compound_tail(page, i);

	prep_compound_head(page, order);
}

/* A block straight from the buddy allocator: no references, no type, no flags. */
static struct page *alloc_frozen(gfp_t gfp, unsigned int order)
{
	unsigned long virt, i;
	struct page *page;

	if (unlikely(!memmap_ready)) {
		lx_mem_setup(sizeof(struct page));
		memmap_ready = true;
	}
	virt = order <= MAX_PAGE_ORDER ? lx_page_alloc(order) : 0;
	if (!virt) {
		if (!(gfp & __GFP_NOWARN))
			pr_warn("page allocation of order %u failed (gfp %#x)\n", order, gfp);
		return NULL;
	}
	page = virt_to_page((void *)virt);
	memset(page, 0, sizeof(*page) << order);
	for (i = 0; i < (1UL << order); i++)
		atomic_set(&page[i]._mapcount, -1);
	if (gfp & __GFP_ZERO)
		memset((void *)virt, 0, PAGE_SIZE << order);
	if (order && (gfp & __GFP_COMP))
		prep_compound_page(page, order);
	return page;
}

struct page *__alloc_frozen_pages_noprof(gfp_t gfp, unsigned int order, int preferred_nid,
					 nodemask_t *nodemask)
{
	return alloc_frozen(gfp, order);
}

struct page *alloc_frozen_pages_nolock_noprof(gfp_t gfp, int nid, unsigned int order)
{
	return alloc_frozen(gfp, order);
}

struct page *__alloc_pages_noprof(gfp_t gfp, unsigned int order, int preferred_nid,
				  nodemask_t *nodemask)
{
	struct page *page = alloc_frozen(gfp, order);

	if (page)
		set_page_refcounted(page);
	return page;
}
EXPORT_SYMBOL(__alloc_pages_noprof);

struct folio *__folio_alloc_noprof(gfp_t gfp, unsigned int order, int preferred_nid,
				   nodemask_t *nodemask)
{
	return page_rmappable_folio(__alloc_pages_noprof(gfp | __GFP_COMP, order, preferred_nid, nodemask));
}
EXPORT_SYMBOL(__folio_alloc_noprof);

unsigned long alloc_pages_bulk_noprof(gfp_t gfp, int preferred_nid, nodemask_t *nodemask,
				      int nr_pages, struct page **page_array)
{
	int i, populated = 0;

	for (i = 0; i < nr_pages; i++) {
		if (!page_array[i])
			page_array[i] = __alloc_pages_noprof(gfp, 0, preferred_nid, nodemask);
		if (!page_array[i])
			break;
		populated++;
	}
	return populated;
}
EXPORT_SYMBOL_GPL(alloc_pages_bulk_noprof);

unsigned long get_free_pages_noprof(gfp_t gfp, unsigned int order)
{
	struct page *page = __alloc_pages_noprof(gfp & ~__GFP_HIGHMEM, order, NUMA_NO_NODE, NULL);

	return page ? (unsigned long)page_address(page) : 0;
}
EXPORT_SYMBOL(get_free_pages_noprof);

/* Return a block whose last reference is gone. */
void free_frozen_pages(struct page *page, unsigned int order)
{
	lx_page_free((unsigned long)page_address(page), order);
}

/*
 * As ___free_pages: a non-compound block whose head is still referenced
 * keeps only the head page, so the tail pages go back now.
 */
void __free_pages(struct page *page, unsigned int order)
{
	int head = PageHead(page);
	unsigned long i;

	if (put_page_testzero(page)) {
		free_frozen_pages(page, order);
	} else if (!head && order) {
		lx_page_split((unsigned long)page_address(page), order);
		for (i = 1; i < (1UL << order); i++)
			free_frozen_pages(page + i, 0);
	}
}
EXPORT_SYMBOL(__free_pages);

void free_pages(unsigned long addr, unsigned int order)
{
	if (addr)
		__free_pages(virt_to_page((void *)addr), order);
}
EXPORT_SYMBOL(free_pages);

void *alloc_pages_exact_noprof(size_t size, gfp_t gfp)
{
	unsigned int order = get_order(size);
	unsigned long nr = DIV_ROUND_UP(size, PAGE_SIZE), i;
	struct page *page = __alloc_pages_noprof(gfp & ~__GFP_COMP, order, NUMA_NO_NODE, NULL);

	if (!page)
		return NULL;
	if (order) {
		lx_page_split((unsigned long)page_address(page), order);
		for (i = 1; i < nr; i++)
			set_page_refcounted(page + i);
		for (i = nr; i < (1UL << order); i++)
			free_frozen_pages(page + i, 0);
	}
	return page_address(page);
}
EXPORT_SYMBOL(alloc_pages_exact_noprof);

void free_pages_exact(void *virt, size_t size)
{
	unsigned long addr = (unsigned long)virt;
	unsigned long end = addr + PAGE_ALIGN(size);

	for (; addr < end; addr += PAGE_SIZE)
		free_page(addr);
}
EXPORT_SYMBOL(free_pages_exact);

/* From mm/swap.c: the last reference to a folio is gone. */
void __folio_put(struct folio *folio)
{
	WARN_ON_ONCE(folio_ref_count(folio));
	free_frozen_pages(&folio->page, folio_order(folio));
}
EXPORT_SYMBOL(__folio_put);

bool is_free_buddy_page(const struct page *page)
{
	return lx_page_is_free((unsigned long)page_address(page));
}
