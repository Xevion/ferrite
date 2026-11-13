/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The shim's PCI calls (ferrite-lkpi `pci`), which the shadow drivers/pci
 * uses. The platform reports a fixed list of devices; an index names one.
 */
#ifndef _LX_PCI_H
#define _LX_PCI_H

#include <linux/pci.h>
#include <linux/types.h>

#define LX_PCI_BAR_NONE 0
#define LX_PCI_BAR_MEM32 1
#define LX_PCI_BAR_MEM64 2
#define LX_PCI_BAR_IO 3
#define LX_PCI_BAR_KIND 0xff
#define LX_PCI_BAR_PREFETCH 0x100

struct lx_pci_bar {
	u64 start;
	u64 size;
	u32 flags;
	u32 pad;
};

struct lx_pci_info {
	u16 segment;
	u8 bus;
	u8 devfn;
	u16 vendor;
	u16 device;
	u16 subsystem_vendor;
	u16 subsystem_device;
	u32 class;
	u8 revision;
	u8 hdr_type;
	u16 pad;
	struct lx_pci_bar bars[6];
};

extern unsigned long lx_pci_device_count(void);
extern int lx_pci_device_info(unsigned long index, struct lx_pci_info *info);
extern int lx_pci_config_read(unsigned long index, unsigned int offset, unsigned int bytes, u32 *value);
extern int lx_pci_config_write(unsigned long index, unsigned int offset, unsigned int bytes, u32 value);
extern int lx_pci_enable(unsigned long index);
extern int lx_pci_set_master(unsigned long index, int on);
extern void lx_irq_set_task(struct task_struct *task);

/* A device the platform reported: its pci_dev and its index there. */
struct lx_pci_dev {
	struct pci_dev pdev;
	unsigned long index;
};

static inline unsigned long lx_pci_index(const struct pci_dev *dev)
{
	return container_of_const(dev, struct lx_pci_dev, pdev)->index;
}

/* Defined in drivers/pci/probe.c: the driver bound to device index, or NULL. */
const char *lx_pci_driver_name(unsigned long index);
/* Defined in drivers/pci/setup-irq.c: run every polled handler once, from the IRQ task. */
void lx_pci_poll_irqs(void);
/* Polls run, and polls whose handlers all returned IRQ_NONE. */
extern unsigned long lx_irq_polls, lx_irq_unhandled;

#endif
