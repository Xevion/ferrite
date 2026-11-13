// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces kernel/dma/mapping.c. All kernel memory is the arena, which the
 * platform made visible to devices once at its bus address, so a DMA address
 * is the page's physical address (__pa is the arena's bus address), mapping
 * is arithmetic, nothing bounces, and x86 coherence makes syncs no-ops.
 */

#include <linux/dma-mapping.h>
#include <linux/gfp.h>
#include <linux/mm.h>
#include <linux/scatterlist.h>

extern unsigned long lx_arena_start, lx_arena_size;

/* The highest bus address in the arena, which every mask must cover. */
static u64 lx_dma_limit(void)
{
	return __pa(lx_arena_start + lx_arena_size - 1);
}

static int lx_dma_check(struct device *dev, u64 mask, const char *what)
{
	if (mask < lx_dma_limit()) {
		dev_err(dev, "%s %#llx does not reach the arena's top bus address %#llx\n", what, mask,
			lx_dma_limit());
		return -EIO;
	}
	return 0;
}

int dma_set_mask(struct device *dev, u64 mask)
{
	int err = lx_dma_check(dev, mask, "DMA mask");

	if (err || !dev->dma_mask)
		return err ? err : -EIO;
	*dev->dma_mask = mask;
	return 0;
}
EXPORT_SYMBOL(dma_set_mask);

int dma_set_coherent_mask(struct device *dev, u64 mask)
{
	int err = lx_dma_check(dev, mask, "coherent DMA mask");

	if (!err)
		dev->coherent_dma_mask = mask;
	return err;
}
EXPORT_SYMBOL(dma_set_coherent_mask);

void *dma_alloc_attrs(struct device *dev, size_t size, dma_addr_t *dma_handle, gfp_t gfp, unsigned long attrs)
{
	struct page *page = alloc_pages(gfp | __GFP_ZERO, get_order(size));

	if (!page) {
		dev_err(dev, "no %zu bytes of coherent DMA memory\n", size);
		return NULL;
	}
	*dma_handle = page_to_phys(page);
	return page_address(page);
}
EXPORT_SYMBOL(dma_alloc_attrs);

void dma_free_attrs(struct device *dev, size_t size, void *cpu_addr, dma_addr_t dma_handle, unsigned long attrs)
{
	if (cpu_addr)
		__free_pages(virt_to_page(cpu_addr), get_order(size));
}
EXPORT_SYMBOL(dma_free_attrs);

dma_addr_t dma_map_page_attrs(struct device *dev, struct page *page, size_t offset, size_t size,
			      enum dma_data_direction dir, unsigned long attrs)
{
	return page_to_phys(page) + offset;
}
EXPORT_SYMBOL(dma_map_page_attrs);

void dma_unmap_page_attrs(struct device *dev, dma_addr_t addr, size_t size, enum dma_data_direction dir,
			  unsigned long attrs)
{
}
EXPORT_SYMBOL(dma_unmap_page_attrs);

int dma_map_sgtable(struct device *dev, struct sg_table *sgt, enum dma_data_direction dir, unsigned long attrs)
{
	struct scatterlist *sg;
	int i;

	for_each_sgtable_sg(sgt, sg, i) {
		sg->dma_address = sg_phys(sg);
		sg_dma_len(sg) = sg->length;
	}
	sgt->nents = sgt->orig_nents;
	return 0;
}
EXPORT_SYMBOL_GPL(dma_map_sgtable);

void dma_unmap_sg_attrs(struct device *dev, struct scatterlist *sg, int nents, enum dma_data_direction dir,
			unsigned long attrs)
{
}
EXPORT_SYMBOL(dma_unmap_sg_attrs);

void __dma_sync_single_for_cpu(struct device *dev, dma_addr_t addr, size_t size, enum dma_data_direction dir)
{
}
EXPORT_SYMBOL(__dma_sync_single_for_cpu);

void __dma_sync_single_for_device(struct device *dev, dma_addr_t addr, size_t size, enum dma_data_direction dir)
{
}
EXPORT_SYMBOL(__dma_sync_single_for_device);
