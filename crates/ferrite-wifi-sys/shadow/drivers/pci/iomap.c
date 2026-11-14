// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/pci/iomap.c and the mapping calls of drivers/pci/devres.c
 * and arch/x86/mm/ioremap.c. Boot services identity map the address space,
 * so a memory BAR's CPU address is its mapping and nothing is ever unmapped.
 * I/O port BARs are not mapped. The lab reports no devices.
 */

#include <linux/device.h>
#include <linux/io.h>
#include <linux/pci.h>

void __iomem *pci_iomap_range(struct pci_dev *dev, int bar, unsigned long offset, unsigned long maxlen)
{
	resource_size_t start;

	if (bar < 0 || bar >= PCI_STD_NUM_BARS || pci_resource_len(dev, bar) <= offset ||
	    !(pci_resource_flags(dev, bar) & IORESOURCE_MEM)) {
		pci_err(dev, "BAR %d is not a mappable memory BAR\n", bar);
		return NULL;
	}
	start = pci_resource_start(dev, bar) + offset;
	pci_info(dev, "BAR %d mapped at its address %pa\n", bar, &start);
	return (void __iomem *)(uintptr_t)start;
}
EXPORT_SYMBOL(pci_iomap_range);

void __iomem *pci_iomap(struct pci_dev *dev, int bar, unsigned long maxlen)
{
	return pci_iomap_range(dev, bar, 0, maxlen);
}
EXPORT_SYMBOL(pci_iomap);

static void lx_pcim_table_release(struct device *dev, void *res)
{
}

/* The per-device table of mapped BARs, kept as a managed resource like Linux's. */
static void __iomem **lx_pcim_table(struct pci_dev *pdev)
{
	void __iomem **table = devres_find(&pdev->dev, lx_pcim_table_release, NULL, NULL);

	if (table)
		return table;
	table = devres_alloc(lx_pcim_table_release, PCI_STD_NUM_BARS * sizeof(*table), GFP_KERNEL);
	if (table)
		devres_add(&pdev->dev, table);
	return table;
}

void __iomem *pcim_iomap(struct pci_dev *pdev, int bar, unsigned long maxlen)
{
	void __iomem **table = lx_pcim_table(pdev);
	void __iomem *addr;

	if (!table || bar < 0 || bar >= PCI_STD_NUM_BARS || table[bar])
		return NULL;
	addr = pci_iomap(pdev, bar, maxlen);
	table[bar] = addr;
	return addr;
}
EXPORT_SYMBOL(pcim_iomap);

void __iomem *const *pcim_iomap_table(struct pci_dev *pdev)
{
	return lx_pcim_table(pdev);
}
EXPORT_SYMBOL(pcim_iomap_table);

int pcim_iomap_regions(struct pci_dev *pdev, int mask, const char *name)
{
	int bar;

	for (bar = 0; bar < PCI_STD_NUM_BARS; bar++) {
		if ((mask & BIT(bar)) && !pcim_iomap(pdev, bar, 0))
			return -ENOMEM;
	}
	return 0;
}
EXPORT_SYMBOL(pcim_iomap_regions);

void iounmap(volatile void __iomem *addr)
{
}
EXPORT_SYMBOL(iounmap);
