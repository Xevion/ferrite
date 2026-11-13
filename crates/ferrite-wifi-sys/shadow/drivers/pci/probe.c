// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/pci/probe.c, search.c and host-bridge.c. Nothing is
 * scanned: the platform reports its devices (ferrite-lkpi `pci`), and a
 * subsys initcall makes one pci_dev per report, below one host bridge, so a
 * driver registered later probes it. After Genode's lx_emul pci_bus.c.
 */

#include <linux/device.h>
#include <linux/init.h>
#include <linux/list.h>
#include <linux/mutex.h>
#include <linux/pci.h>
#include <linux/slab.h>

#include <lx/pci.h>
#include "pci.h"

static struct device lx_pci_host;
static LIST_HEAD(lx_pci_buses);
static struct lx_pci_dev **lx_pci_devs;
static unsigned long lx_pci_count;
static DEFINE_MUTEX(lx_pci_rescan_remove);

struct device *pci_get_host_bridge_device(struct pci_dev *dev)
{
	return get_device(&lx_pci_host);
}

void pci_put_host_bridge_device(struct device *dev)
{
	put_device(dev);
}

static struct pci_bus *lx_pci_bus(u16 segment, u8 number)
{
	struct pci_sysdata *sd;
	struct pci_bus *bus;

	list_for_each_entry(bus, &lx_pci_buses, node) {
		if (bus->number == number && pci_domain_nr(bus) == segment)
			return bus;
	}
	bus = kzalloc(sizeof(*bus), GFP_KERNEL);
	sd = kzalloc(sizeof(*sd), GFP_KERNEL);
	if (!bus || !sd) {
		kfree(bus);
		kfree(sd);
		return NULL;
	}
	sd->domain = segment;
	bus->sysdata = sd;
	bus->number = number;
	bus->bridge = &lx_pci_host;
	INIT_LIST_HEAD(&bus->node);
	INIT_LIST_HEAD(&bus->children);
	INIT_LIST_HEAD(&bus->devices);
	INIT_LIST_HEAD(&bus->slots);
	INIT_LIST_HEAD(&bus->resources);
	bus->max_bus_speed = PCI_SPEED_UNKNOWN;
	bus->cur_bus_speed = PCI_SPEED_UNKNOWN;
	list_add_tail(&bus->node, &lx_pci_buses);
	return bus;
}

static void lx_pci_release(struct device *dev)
{
	/* The devices live as long as the program; a final put is a refcount bug. */
	pr_err("pci %s: last reference dropped\n", dev_name(dev));
}

static void lx_pci_resources(struct pci_dev *dev, const struct lx_pci_info *info)
{
	int i;

	for (i = 0; i < 6; i++) {
		const struct lx_pci_bar *bar = &info->bars[i];
		struct resource *res = &dev->resource[i];

		if ((bar->flags & LX_PCI_BAR_KIND) == LX_PCI_BAR_NONE || !bar->size)
			continue;
		res->name = pci_name(dev);
		res->start = bar->start;
		res->end = bar->start + bar->size - 1;
		switch (bar->flags & LX_PCI_BAR_KIND) {
		case LX_PCI_BAR_IO:
			res->flags = IORESOURCE_IO;
			break;
		case LX_PCI_BAR_MEM64:
			res->flags = IORESOURCE_MEM | IORESOURCE_MEM_64;
			break;
		default:
			res->flags = IORESOURCE_MEM;
		}
		if (bar->flags & LX_PCI_BAR_PREFETCH)
			res->flags |= IORESOURCE_PREFETCH;
		pci_info(dev, "BAR %d: %pR\n", i, res);
	}
}

static struct lx_pci_dev *lx_pci_add(unsigned long index)
{
	struct lx_pci_info info;
	struct lx_pci_dev *lx;
	struct pci_dev *dev;
	struct pci_bus *bus;
	int pos;

	if (lx_pci_device_info(index, &info))
		return NULL;
	bus = lx_pci_bus(info.segment, info.bus);
	lx = kzalloc(sizeof(*lx), GFP_KERNEL);
	if (!bus || !lx) {
		kfree(lx);
		return NULL;
	}
	lx->index = index;
	dev = &lx->pdev;
	INIT_LIST_HEAD(&dev->bus_list);
	dev->bus = bus;
	dev->sysdata = bus->sysdata;
	dev->devfn = info.devfn;
	dev->vendor = info.vendor;
	dev->device = info.device;
	dev->subsystem_vendor = info.subsystem_vendor;
	dev->subsystem_device = info.subsystem_device;
	dev->class = info.class;
	dev->revision = info.revision;
	dev->hdr_type = info.hdr_type & 0x7f;
	dev->multifunction = !!(info.hdr_type & 0x80);
	dev->current_state = PCI_D0;
	dev->error_state = pci_channel_io_normal;
	dev->dma_mask = DMA_BIT_MASK(32);
	dev->cfg_size = PCI_CFG_SPACE_SIZE;

	device_initialize(&dev->dev);
	dev->dev.release = lx_pci_release;
	dev->dev.bus = &pci_bus_type;
	dev->dev.parent = bus->bridge;
	dev->dev.dma_mask = &dev->dma_mask;
	dev->dev.coherent_dma_mask = DMA_BIT_MASK(32);
	dev_set_name(&dev->dev, "%04x:%02x:%02x.%d", info.segment, info.bus, PCI_SLOT(info.devfn),
		     PCI_FUNC(info.devfn));

	pos = pci_find_capability(dev, PCI_CAP_ID_EXP);
	if (pos) {
		dev->pcie_cap = pos;
		pci_read_config_word(dev, pos + PCI_EXP_FLAGS, &dev->pcie_flags_reg);
		dev->cfg_size = PCI_CFG_SPACE_EXP_SIZE;
	}
	lx_pci_resources(dev, &info);
	list_add_tail(&dev->bus_list, &bus->devices);
	pci_info(dev, "[%04x:%04x] subsystem [%04x:%04x] class %06x rev %02x%s\n", dev->vendor, dev->device,
		 dev->subsystem_vendor, dev->subsystem_device, dev->class, dev->revision,
		 pos ? " PCIe" : "");

	pci_dev_allow_binding(dev);
	if (device_add(&dev->dev)) {
		pci_err(dev, "device_add failed\n");
		list_del(&dev->bus_list);
		return NULL;
	}
	return lx;
}

static int __init lx_pci_init(void)
{
	unsigned long i, count = lx_pci_device_count();
	int err;

	device_initialize(&lx_pci_host);
	dev_set_name(&lx_pci_host, "pci0000:00");
	err = device_add(&lx_pci_host);
	if (err)
		return err;
	if (!count)
		return 0;
	lx_pci_devs = kcalloc(count, sizeof(*lx_pci_devs), GFP_KERNEL);
	if (!lx_pci_devs)
		return -ENOMEM;
	for (i = 0; i < count; i++) {
		lx_pci_devs[i] = lx_pci_add(i);
		if (lx_pci_devs[i])
			lx_pci_count = i + 1;
	}
	return 0;
}
subsys_initcall(lx_pci_init);

/* Each found device is returned with a reference, and the one passed in as from is put. */
struct pci_dev *pci_get_device(unsigned int vendor, unsigned int device, struct pci_dev *from)
{
	unsigned long i = from ? lx_pci_index(from) + 1 : 0;
	struct pci_dev *found = NULL;

	for (; i < lx_pci_count && !found; i++) {
		struct pci_dev *dev = lx_pci_devs[i] ? &lx_pci_devs[i]->pdev : NULL;

		if (dev && (vendor == PCI_ANY_ID || dev->vendor == vendor) &&
		    (device == PCI_ANY_ID || dev->device == device))
			found = pci_dev_get(dev);
	}
	pci_dev_put(from);
	return found;
}
EXPORT_SYMBOL(pci_get_device);

struct pci_dev *pci_get_slot(struct pci_bus *bus, unsigned int devfn)
{
	struct pci_dev *dev;

	list_for_each_entry(dev, &bus->devices, bus_list) {
		if (dev->devfn == devfn)
			return pci_dev_get(dev);
	}
	return NULL;
}
EXPORT_SYMBOL(pci_get_slot);

void pci_lock_rescan_remove(void)
{
	mutex_lock(&lx_pci_rescan_remove);
}
EXPORT_SYMBOL_GPL(pci_lock_rescan_remove);

void pci_unlock_rescan_remove(void)
{
	mutex_unlock(&lx_pci_rescan_remove);
}
EXPORT_SYMBOL_GPL(pci_unlock_rescan_remove);

/* A device is never taken off the list: removing it unbinds its driver and leaves it parked. */
void pci_stop_and_remove_bus_device(struct pci_dev *dev)
{
	pci_info(dev, "removal requested; unbinding the driver\n");
	device_release_driver(&dev->dev);
	lx_pci_set_master(lx_pci_index(dev), 0);
}
EXPORT_SYMBOL(pci_stop_and_remove_bus_device);

/* The platform's list is fixed, so a rescan finds nothing new. */
unsigned int pci_rescan_bus(struct pci_bus *bus)
{
	pr_info("pci: rescan of bus %02x requested; the device list is fixed\n", bus->number);
	return 0;
}
EXPORT_SYMBOL_GPL(pci_rescan_bus);

/* The name of the driver bound to device index, or NULL when none is. */
const char *lx_pci_driver_name(unsigned long index)
{
	struct device_driver *drv;

	if (index >= lx_pci_count || !lx_pci_devs[index])
		return NULL;
	drv = READ_ONCE(lx_pci_devs[index]->pdev.dev.driver);
	return drv ? drv->name : NULL;
}
